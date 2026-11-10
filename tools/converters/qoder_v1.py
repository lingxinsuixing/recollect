#!/usr/bin/env python3
"""Qoder/Claude 同形会话 -> ExchangeFile v1（分段保真版）。

类型闭集（操作员 2026-10-30 口径）：
  human          —— 主人说的话
  agent          —— 主 agent 与子 agent 同一角色（子 agent 不单列说话人）
  tool_call      —— 工具调用（system 轮）
  tool_result    —— 工具结果（system 轮，可带 error）
  system         —— 系统提示：压缩边界、压缩摘要、运行时提示、错误/权限/中断

映射规则：
  type=user/assistant, content[].type=text      -> human / agent
  type=user/assistant, content[].type=thinking  -> 丢弃（计数）
  content[].type=tool_use                       -> system，"[tool_call name=… id=…] <args>"
  content[].type=tool_result                    -> system，"[tool_result[ error] for=…] <content>"
  任意记录 isCompactSummary=true                 -> system，正文以 "[[compact_summary]]" 开头
                                                  （交给抽取侧既有压缩闸，绝不冒充 human）
  type=system                                   -> system，"[system:<subtype>] <content>"
                                                  压缩边界附 messages/pre/post token 读数

不做：摘要、改写、合并、按长度截断、抽样。脱敏交给核心 import。
确定性：按源行序输出；同一输入两次运行逐字节相同。
"""
import argparse
import datetime
import hashlib
import json
import os
import re


def parse_ts_ms(s):
    if s is None:
        return None
    if isinstance(s, (int, float)):
        return int(s)
    s = str(s).strip()
    if s.endswith('Z'):
        s = s[:-1] + '+00:00'
    try:
        dt = datetime.datetime.fromisoformat(s)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=datetime.timezone.utc)
    return int(dt.timestamp() * 1000)


def block_text(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        out = []
        for b in content:
            if isinstance(b, dict) and b.get('text'):
                out.append(b['text'])
            elif isinstance(b, str):
                out.append(b)
        return '\n'.join(out)
    if content is None:
        return ''
    return json.dumps(content, ensure_ascii=False, sort_keys=True)


def project_from_dir(path):
    """把会话记录里的工作区路径折成逻辑项目名（无分隔符、无盘符、≤64）。

    会话 jsonl 没有显式项目名，只有 `directories`（工作区绝对路径）。取 basename，
    非法字符换 '-'，空则回落 'qoder'。这样 19 个 Qoder 项目各自成 scope，
    不会全部挤进 'recollect'。
    """
    if not path:
        return 'qoder'
    base = os.path.basename(path.replace('\\', '/').rstrip('/'))
    out = ''.join(c if (c.isalnum() or c in '._-') else '-' for c in base).strip('-')
    return out[:64] or 'qoder'


def convert(path, args):
    proc = {
        'text_human': 0, 'text_agent': 0, 'tool_use': 0, 'tool_result': 0,
        'tool_result_error': 0, 'compact_summary': 0,
        'thinking_skipped': 0, 'empty_skipped': 0, 'other_blocks': 0,
    }
    turns = []
    meta = None
    last_ts = None
    agent_calls = {}   # tool_use id -> 子 agent 编码（Agent 工具的 name，回落到 id）
    sidechain_codes = {}  # parentUuid -> 子 agent 编码（sidechain 会话，按首次出现编号）

    with open(path, encoding='utf-8') as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                d = json.loads(line)
            except Exception:
                proc['unparsable'] = proc.get('unparsable', 0) + 1
                continue
            if meta is None:
                project = args.project
                if getattr(args, 'project_mode', 'fixed') == 'dir':
                    dirs = d.get('directories') or []
                    project = project_from_dir(dirs[0] if dirs else None)
                meta = {
                    'session_key': '%s:%s' % (args.source_agent,
                                              d.get('sessionId') or os.path.basename(path).split('.')[0]),
                    'device_id': args.device,
                    'source_agent': args.source_agent,
                    'machine': args.machine,
                    'project_hint': project,
                }
            t = d.get('type')
            ts = parse_ts_ms(d.get('timestamp'))
            if ts is not None:
                last_ts = ts

            # ① 系统记录（压缩边界 / 运行时提示）
            if t == 'system':
                sub = d.get('subtype') or 'notice'
                body = '[system:%s] %s' % (sub, (d.get('content') or '').strip())
                cm = d.get('compactMetadata') or {}
                if cm:
                    body += ' (messages=%s pre_tokens=%s post_tokens=%s)' % (
                        cm.get('messagesSummarized'), cm.get('preTokens'), cm.get('postTokens'))
                turns.append({'role': 'system', 'body': body, 'created_at_ms': ts})
                proc['system:' + str(sub)] = proc.get('system:' + str(sub), 0) + 1
                continue
            if t not in ('user', 'assistant'):
                proc['meta:' + str(t)] = proc.get('meta:' + str(t), 0) + 1
                continue

            msg = d.get('message') or {}
            role = 'human' if msg.get('role') == 'user' else 'agent'
            # isSidechain：同一个子 agent 的连续记录共用一个编码（按 parentUuid 归并）。
            side_speaker = None
            if d.get('isSidechain') and role == 'agent':
                key = d.get('parentUuid') or d.get('logicalParentUuid') or 'root'
                if key not in sidechain_codes:
                    sidechain_codes[key] = 'sidechain-%d' % (len(sidechain_codes) + 1)
                side_speaker = 'subagent:%s' % sidechain_codes[key]
            content = msg.get('content')
            blocks = content if isinstance(content, list) else [{'type': 'text', 'text': content}]

            # ② 压缩摘要：源里伪装成 user 轮，必须改判 system
            if d.get('isCompactSummary'):
                text = '\n'.join(b.get('text', '') for b in blocks
                                 if isinstance(b, dict) and b.get('type') == 'text')
                if text.strip():
                    turns.append({'role': 'system',
                                  'body': '[[compact_summary]]\n' + text,
                                  'created_at_ms': ts})
                    proc['compact_summary'] += 1
                else:
                    proc['empty_skipped'] += 1
                continue

            for b in blocks:
                if isinstance(b, str):
                    b = {'type': 'text', 'text': b}
                if not isinstance(b, dict):
                    continue
                k = b.get('type')
                out_role = role
                if k == 'text':
                    body = b.get('text') or ''
                    if role == 'human' and body.startswith('[Request interrupted'):
                        # 应用插入的中断通知：不是人类发言。
                        turns.append({'role': 'system', 'body': '[system:interrupted] ' + body,
                                      'created_at_ms': ts})
                        proc['system:interrupted'] = proc.get('system:interrupted', 0) + 1
                        continue
                    if role == 'human' and (body.startswith('[SYSTEM NOTIFICATION')
                                            or '<task-notification>' in body):
                        # 子 agent 完成回执：源里是 user 轮，实为子 agent 的报告。
                        m = re.search(r'<tool-use-id>([^<]+)</tool-use-id>', body)
                        code = agent_calls.get(m.group(1)) if m else None
                        if code is None:
                            m2 = re.search(r'<task-id>([^<]+)</task-id>', body)
                            code = (m2.group(1) if m2 else 'unknown')
                        turns.append({'role': 'agent', 'body': body, 'created_at_ms': ts,
                                      'speaker': 'subagent:%s' % code})
                        proc['subagent_notification'] = proc.get('subagent_notification', 0) + 1
                        continue
                    proc['text_human' if role == 'human' else 'text_agent'] += 1
                elif k == 'thinking':
                    proc['thinking_skipped'] += 1
                    continue
                elif k == 'tool_use':
                    if b.get('name') == 'Agent':
                        # 子 agent 实体：本会话内唯一编码，取 Agent 调用的 name，回落到调用 id。
                        code = ((b.get('input') or {}).get('name') or b.get('id'))
                        agent_calls[b.get('id')] = code
                    body = '[tool_call name=%s id=%s] %s' % (
                        b.get('name'), b.get('id'),
                        json.dumps(b.get('input'), ensure_ascii=False, sort_keys=True))
                    out_role = 'system'
                    proc['tool_use'] += 1
                elif k == 'tool_result':
                    flag = ' error' if b.get('is_error') else ''
                    body = '[tool_result%s for=%s] %s' % (flag, b.get('tool_use_id'), block_text(b.get('content')))
                    out_role = 'system'
                    proc['tool_result'] += 1
                    if b.get('is_error'):
                        proc['tool_result_error'] += 1
                else:
                    proc['other_blocks'] += 1
                    continue

                if not body.strip():
                    proc['empty_skipped'] += 1
                    continue
                turn = {'role': out_role, 'body': body, 'created_at_ms': ts}
                if side_speaker and out_role == 'agent':
                    turn['speaker'] = side_speaker
                    proc['sidechain_turns'] = proc.get('sidechain_turns', 0) + 1
                turns.append(turn)

    if meta is None:
        raise SystemExit('no user/assistant/system records found')
    meta['last_saved_at_ms'] = last_ts
    meta['turns'] = turns
    return {'format_version': 1, 'sessions': [meta]}, proc


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('input')
    ap.add_argument('--out-dir', required=True)
    ap.add_argument('--device', default='local-qoder')
    ap.add_argument('--source-agent', default='qoder')
    ap.add_argument('--machine', default='local')
    ap.add_argument('--project', default='recollect')
    ap.add_argument('--project-mode', choices=('fixed', 'dir'), default='fixed',
                    help='fixed=用 --project；dir=按会话 directories 的 basename 推逻辑项目名')
    args = ap.parse_args()

    doc, proc = convert(args.input, args)
    os.makedirs(args.out_dir, exist_ok=True)
    stem = os.path.splitext(os.path.basename(args.input))[0]
    payload = json.dumps(doc, ensure_ascii=False, sort_keys=True, indent=1)
    with open(os.path.join(args.out_dir, stem + '.v1.json'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(payload)
    manifest = dict(proc)
    manifest.update({
        'input': os.path.abspath(args.input),
        'bytes': os.path.getsize(args.input),
        'turns_out': len(doc['sessions'][0]['turns']),
        'sha256': hashlib.sha256(payload.encode('utf-8')).hexdigest(),
    })
    with open(os.path.join(args.out_dir, stem + '.manifest.json'), 'w', encoding='utf-8', newline='\n') as f:
        f.write(json.dumps(manifest, ensure_ascii=False, indent=1, sort_keys=True))
    print(json.dumps(manifest, ensure_ascii=False))


if __name__ == '__main__':
    main()
