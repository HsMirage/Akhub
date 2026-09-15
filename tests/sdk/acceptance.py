#!/usr/bin/env python3
"""Akhub 官方 SDK 黑盒验收（计划 §26.2）。

用官方 OpenAI / Anthropic SDK 作为客户端，对一个 Akhub 入口（或上游站点做
基线自检）跑一遍协议矩阵：非流式、流式、工具调用、Responses 状态链、模型
列表、Token 计数、图片生成与错误对象/状态码。

用法（不把密钥写进文件）：

    python3 tests/sdk/acceptance.py \
        --base-url https://akhub-89-58-17-138.nip.io \
        --key "$AKHUB_TEST_KEY" \
        --chat-model glm-5.3-flash \
        --responses-model glm-5.3-flash \
        --messages-model glm-5.3-flash \
        --count-tokens \
        --image-model gpt-image-1

只跑子集：--suite openai / anthropic / images；上游不支持的能力会自动 SKIP，
用 --strict 把 SKIP 也算失败。
"""

from __future__ import annotations

import argparse
import os
import sys
import time
import traceback

RESULTS: list[tuple[str, str, str]] = []  # (状态, 名称, 备注)


def record(status: str, name: str, note: str = "") -> None:
    RESULTS.append((status, name, note))
    mark = {"PASS": "✅", "FAIL": "❌", "SKIP": "⏭️"}.get(status, "?")
    line = f"{mark} {name}"
    if note:
        line += f" — {note}"
    print(line, flush=True)


class Unsupported(Exception):
    """上游没有提供这项能力：记为 SKIP 而不是失败。"""


def case(name, fn, strict: bool = True):
    """跑一个用例；返回 None 表示上游不支持，记为 SKIP。"""
    started = time.time()
    try:
        result = fn()
    except Unsupported as error:
        record("FAIL" if strict else "SKIP", name, f"上游未提供：{error}")
        return
    except Exception as error:  # noqa: BLE001 - 验收脚本要显示所有失败
        record("FAIL", name, f"{type(error).__name__}: {error}")
        return
    elapsed = time.time() - started
    if result is None:
        record("SKIP" if not strict else "FAIL", name, f"{elapsed:.1f}s（上游未提供）")
    else:
        record("PASS", name, f"{elapsed:.1f}s")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-url", default=os.environ.get("AKHUB_BASE_URL", ""))
    parser.add_argument("--key", default=os.environ.get("AKHUB_TEST_KEY", ""))
    parser.add_argument("--chat-model")
    parser.add_argument("--responses-model")
    parser.add_argument("--messages-model")
    parser.add_argument("--image-model")
    parser.add_argument("--count-tokens", action="store_true")
    parser.add_argument("--suite", choices=["all", "openai", "anthropic", "images"], default="all")
    parser.add_argument("--strict", action="store_true", help="SKIP 也算失败")
    args = parser.parse_args()

    if not args.base_url or not args.key:
        print("需要 --base-url 与 --key（或环境变量 AKHUB_BASE_URL / AKHUB_TEST_KEY）")
        return 2

    from anthropic import Anthropic
    from openai import OpenAI

    # OpenAI SDK 要求 base_url 自带 /v1；Anthropic SDK 要求根地址。
    # 两个 SDK 因此不能共用同一个字符串，这里按各自约定归一化。
    root = args.base_url.rstrip("/")
    openai_base = root if root.endswith("/v1") else f"{root}/v1"
    anthropic_base = root[: -len("/v1")] if root.endswith("/v1") else root

    openai_client = OpenAI(base_url=openai_base, api_key=args.key, timeout=90, max_retries=0)
    anthropic_client = Anthropic(
        base_url=anthropic_base, api_key=args.key, timeout=90, max_retries=0
    )
    strict = args.strict

    def want(suite: str) -> bool:
        return args.suite in ("all", suite)

    # ------------------------------------------------------------ OpenAI

    if want("openai"):
        case("OpenAI：模型列表可用", lambda: openai_client.models.list().data, strict)

        if args.chat_model:
            chat = args.chat_model

            def chat_once():
                response = openai_client.chat.completions.create(
                    model=chat,
                    messages=[{"role": "user", "content": "只回答两个字：你好"}],
                    max_tokens=64,
                )
                assert response.choices, "没有 choices"
                assert response.choices[0].message.role == "assistant"
                return response

            case(f"OpenAI Chat 非流式（{chat}）", chat_once, strict)

            def chat_stream():
                stream = openai_client.chat.completions.create(
                    model=chat,
                    messages=[{"role": "user", "content": "数到三，只输出数字"}],
                    max_tokens=256,
                    stream=True,
                )
                content = ""
                reasoning = ""
                finished = False
                for chunk in stream:
                    if not chunk.choices:
                        continue
                    delta = chunk.choices[0].delta
                    content += getattr(delta, "content", None) or ""
                    reasoning += getattr(delta, "reasoning_content", None) or ""
                    if chunk.choices[0].finish_reason:
                        finished = True
                assert finished, "流没有正常结束"
                # 推理模型可能整段只产出思考增量：这也算流式链路通了。
                assert content or reasoning, "没有收到任何流式增量"
                return "content" if content else "reasoning"

            case(f"OpenAI Chat 流式（{chat}）", chat_stream, strict)

            def chat_tools():
                tool = {
                    "type": "function",
                    "function": {
                        "name": "get_time",
                        "description": "查询指定城市的当前时间",
                        "parameters": {
                            "type": "object",
                            "properties": {"city": {"type": "string"}},
                            "required": ["city"],
                        },
                    },
                }
                first = openai_client.chat.completions.create(
                    model=chat,
                    messages=[{"role": "user", "content": "调用 get_time 查询北京的时间，不要直接回答"}],
                    tools=[tool],
                    max_tokens=128,
                )
                calls = first.choices[0].message.tool_calls or []
                if not calls:
                    raise AssertionError("模型没有发起工具调用")
                call = calls[0]
                second = openai_client.chat.completions.create(
                    model=chat,
                    messages=[
                        {"role": "user", "content": "调用 get_time 查询北京的时间"},
                        first.choices[0].message,
                        {
                            "role": "tool",
                            "tool_call_id": call.id,
                            "content": '{"time": "12:00"}',
                        },
                    ],
                    tools=[tool],
                    max_tokens=128,
                )
                assert second.choices[0].message.content is not None
                return call.function.name

            case(f"OpenAI Chat 工具调用往返（{chat}）", chat_tools, strict)

        if args.responses_model:
            model = args.responses_model

            def responses_once():
                response = openai_client.responses.create(
                    model=model, input="只回答两个字：你好", max_output_tokens=128
                )
                assert response.id, "响应缺少 id"
                assert response.id.startswith("resp_"), response.id
                assert response.status in ("completed", "incomplete"), response.status
                return response

            def responses_stream():
                stream = openai_client.responses.create(
                    model=model, input="数到三", max_output_tokens=128, stream=True
                )
                events = []
                for event in stream:
                    events.append(event.type)
                    if event.type == "response.completed":
                        assert getattr(event.response, "id", "")
                        break
                assert "response.created" in events, events[:5]
                return events[-1]

            def responses_retrieve():
                created = responses_once()
                got = openai_client.responses.retrieve(created.id)
                assert got.id == created.id
                return got.id

            def responses_continue():
                created = responses_once()
                follow = openai_client.responses.create(
                    model=model,
                    previous_response_id=created.id,
                    input="那 1+1 等于几",
                    max_output_tokens=128,
                )
                assert follow.id != created.id
                return follow.id

            def responses_error_shape():
                try:
                    openai_client.responses.create(
                        model="definitely-not-a-real-model-xyz", input="hi"
                    )
                except Exception as error:  # noqa: BLE001
                    status = getattr(error, "status_code", None)
                    assert status in (400, 404), f"状态码 {status}"
                    return status
                raise AssertionError("不存在的模型必须报错")

            case(f"OpenAI Responses 非流式（{model}）", responses_once, strict)
            case(f"OpenAI Responses 流式（{model}）", responses_stream, strict)
            case(f"OpenAI Responses 查询（{model}）", responses_retrieve, strict)
            case(f"OpenAI Responses 续链（{model}）", responses_continue, strict)
            case("OpenAI 错误对象与状态码", responses_error_shape, strict)

        def auth_error():
            import openai

            bad = openai.OpenAI(
                base_url=openai_base, api_key="sk-not-a-real-key", timeout=30, max_retries=0
            )
            try:
                bad.models.list()
            except Exception as error:  # noqa: BLE001
                status = getattr(error, "status_code", None)
                assert status == 401, f"状态码 {status}"
                return status
            raise AssertionError("无效 Key 必须 401")

        case("OpenAI 无效 Key → 401", auth_error, strict)

        def models_get():
            model = args.chat_model or args.responses_model
            got = openai_client.models.retrieve(model)
            if not getattr(got, "id", None):
                raise Unsupported("GET /v1/models/{model} 未实现或形状不符")
            assert got.id == model
            return got.id

        if args.chat_model or args.responses_model:
            case("GET /v1/models/{model}", models_get, strict)

    # ------------------------------------------------------------ Anthropic

    if want("anthropic") and args.messages_model:
        model = args.messages_model

        def messages_once():
            message = anthropic_client.messages.create(
                model=model,
                max_tokens=64,
                messages=[{"role": "user", "content": "只回答两个字：你好"}],
            )
            assert message.role == "assistant"
            assert message.content, "没有内容块"
            return message

        def messages_stream():
            text = ""
            thinking = ""
            finished = False
            with anthropic_client.messages.stream(
                model=model,
                max_tokens=256,
                messages=[{"role": "user", "content": "数到三"}],
            ) as stream:
                for event in stream:
                    if event.type == "content_block_delta":
                        delta = event.delta
                        text += getattr(delta, "text", None) or ""
                        thinking += getattr(delta, "thinking", None) or ""
                    if event.type == "message_stop":
                        finished = True
            assert finished, "流没有正常结束"
            # 推理模型可能整段只产出 thinking 增量。
            assert text or thinking, "流式响应既没有文本也没有思考增量"
            return "text" if text else "thinking"

        def messages_tools():
            tool = {
                "name": "get_time",
                "description": "查询指定城市的当前时间",
                "input_schema": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                },
            }
            first = anthropic_client.messages.create(
                model=model,
                max_tokens=256,
                tools=[tool],
                messages=[{"role": "user", "content": "调用 get_time 查询北京的时间"}],
            )
            calls = [block for block in first.content if block.type == "tool_use"]
            if not calls:
                raise AssertionError("模型没有发起工具调用")
            call = calls[0]
            second = anthropic_client.messages.create(
                model=model,
                max_tokens=256,
                tools=[tool],
                messages=[
                    {"role": "user", "content": "调用 get_time 查询北京的时间"},
                    {"role": "assistant", "content": first.content},
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": call.id,
                                "content": '{"time": "12:00"}',
                            }
                        ],
                    },
                ],
            )
            assert second.content, "工具结果回合没有内容"
            return call.name

        case(f"Anthropic Messages 非流式（{model}）", messages_once, strict)
        case(f"Anthropic Messages 流式（{model}）", messages_stream, strict)
        case(f"Anthropic Messages 工具调用往返（{model}）", messages_tools, strict)

        if args.count_tokens:

            def count_tokens():
                counted = anthropic_client.messages.count_tokens(
                    model=model,
                    messages=[{"role": "user", "content": "只回答两个字：你好"}],
                )
                assert counted.input_tokens > 0
                return counted.input_tokens

            case(f"Anthropic count_tokens（{model}）", count_tokens, strict)

    # ------------------------------------------------------------ Images

    if want("images") and args.image_model:
        model = args.image_model

        def image_generate():
            result = openai_client.images.generate(
                model=model, prompt="a tiny red dot on a white background", n=1, size="1024x1024"
            )
            assert result.data, "没有图片数据"
            item = result.data[0]
            if not (getattr(item, "b64_json", None) or getattr(item, "url", None)):
                raise AssertionError("既没有 b64_json 也没有 url")
            return "b64_json" if getattr(item, "b64_json", None) else "url"

        case(f"OpenAI Images 生成（{model}）", image_generate, strict)

    print()
    passed = sum(1 for status, _, _ in RESULTS if status == "PASS")
    failed = sum(1 for status, _, _ in RESULTS if status == "FAIL")
    skipped = sum(1 for status, _, _ in RESULTS if status == "SKIP")
    print(f"结果：{passed} 通过 / {failed} 失败 / {skipped} 跳过")
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception:  # noqa: BLE001
        traceback.print_exc()
        sys.exit(2)
