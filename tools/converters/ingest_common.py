"""转换器共用：脱敏、时间、内容块提取。只处理文本，不打印正文。

这一份是本仓库内的移植版；与外部工作区的同名文件保持同形，
新增来源时优先复用这里的 `scrub`，不要各写一套正则。
"""
import json
import re
from datetime import datetime, timezone

SECRET_PATTERNS = [
    (re.compile(r"sk-[A-Za-z0-9_-]{10,}"), "<redacted:api_key>", "api_key"),
    (re.compile(r"cpk-[A-Za-z0-9_-]{10,}"), "<redacted:api_key>", "api_key"),
    (re.compile(r"AKIA[0-9A-Z]{16}"), "<redacted:aws_key>", "aws_key"),
    (re.compile(r"Bearer\s+[A-Za-z0-9._\-]{10,}"), "Bearer <redacted:token>", "token"),
    (re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
     "<redacted:private_key>", "private_key"),
    (re.compile(r"(password|passwd|口令|密码)\s*[:：=]\s*\S+", re.IGNORECASE),
     r"\1: <redacted:password>", "password"),
]
PII_PATTERNS = [
    (re.compile(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"), "<redacted:email>", "email"),
    (re.compile(r"(?<!\d)1[3-9]\d{9}(?!\d)"), "<redacted:phone>", "phone"),
    (re.compile(r"(?<!\d)\d{17}[\dXx](?!\d)"), "<redacted:id>", "id"),
]


def scrub(text, counts):
    if not isinstance(text, str):
        return text
    for pat, repl, kind in SECRET_PATTERNS + PII_PATTERNS:
        text, n = pat.subn(repl, text)
        if n:
            counts[kind] = counts.get(kind, 0) + n
    return text


def scrub_obj(obj, counts):
    if isinstance(obj, str):
        return scrub(obj, counts)
    if isinstance(obj, list):
        return [scrub_obj(x, counts) for x in obj]
    if isinstance(obj, dict):
        return {k: scrub_obj(v, counts) for k, v in obj.items()}
    return obj


def iso_ms(ts):
    if not isinstance(ts, str) or not ts:
        return None
    try:
        dt = datetime.fromisoformat(ts.replace("Z", "+00:00"))
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return int(dt.timestamp() * 1000)
    except ValueError:
        return None


def block_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for b in content:
            if isinstance(b, dict) and b.get("type") == "text":
                parts.append(b.get("text", ""))
            elif isinstance(b, str):
                parts.append(b)
        return "\n".join(p for p in parts if p)
    return ""
