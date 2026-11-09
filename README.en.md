# recollect

English | [简体中文](README.md)

A memory service for agents: **raw turns in one layer, distilled memory in another**. Switch sessions, switch machines, or go offline — your agent can still pick up where it left off.

> Status: this is the version I personally tested (v0.1.0). Issues and PRs are welcome. No fixed release cadence and no response-time promise.

## What it solves

- **Session resets wipe the context.** Decisions, blockers and promises from yesterday are gone.
- **Every agent starts from zero.** The same person's preferences have to be re-explained to each agent.
- **Memory is a black box.** You get a conclusion, but not the original words or who actually said them.

## When it is useful

- Continue yesterday's work: let the agent recall the last decision, progress and open traps.
- Several agents share one "me": IDE, terminal and mobile agents read the same memory.
- Trace back to the source: expand a memory card all the way to the turn it came from.
- Multi-machine LAN: the centre runs at home; a laptop can read/write while away and merge later.
- Just remember a sentence: write it down; extraction and summaries happen in the background.

## Design choices

1. **Raw turns and memory are separate.** The transcript layer keeps verbatim text; the memory layer keeps cards, facts and events. Both are traceable. A summary is an index, not a source of truth.
2. **Centre plus edge.** The centre is authoritative; an edge holds a read-only replica, can self-distill offline, and merges back later.
3. **One read gate.** Agents of the same owner can read each other by default; environment declarations stay agent-private; the transcript surface is open to paired devices on the LAN. That is an explicit trade-off, not a missing check.
4. **The agent does not decide for you.** Recall returns evidence, time and provenance. Voting, retirement and deletion stay with the human.

## What it feels like

- **Single binary, zero install**: SQLite plus built-in HTTP/TLS, no external database or queue.
- **Chinese-friendly**: jieba tokenization; search Chinese raw text directly.
- **MCP native**: `recollect_*` memory/transcript tools and `core_*` always-on tools.
- **Auditable**: every memory row has provenance; report `reuse`, `report` mistakes.
- **Multi-machine without pain**: `hub serve` at the centre, `node pair` at the edge; offline staging merges when the centre is back.

## Quick start

1. Download from [Releases](https://github.com/lingxinsuixing/recollect/releases):
   - Windows x64: `setup.exe` (per-user) or portable `zip`
   - Linux x64: `deb` / `rpm` or portable `tar.gz` (musl static)
2. Verify `SHA256SUMS`; use `manifest.json` to pick an asset.
3. Start the centre with `recollect-hub serve`; choose a data directory with `--data-dir`.
4. **The database starts empty**; import history with `recollect-hub import`.

## For agents (short version)

MCP startup needs three things: data directory, device name, agent name; owner is optional.

```json
{"command":"/abs/bin/recollect-mcp","args":[
  "--data-dir","/abs/data","--device-id","local-xxx",
  "--agent-id","your-agent","--principal-id","p-owner"]}
```

Three common paths:

- Catch up: `recollect_recall(query="…")`; use `view="expand"` to get `turn:<uid>`.
- Find raw words: `recollect_search(q="…")` or `recollect_turn(turn_uid="…", before=2, after=2)`.
- Remember something: `recollect_remember(body="…")`; extraction is asynchronous.

Empty results are normal: `no_rows` means "no match this time"; `no_memory` means "this node has no replica yet".
Details (importers, auto-import, setup) are in the [agent guide](docs/2026-11-09-agent-install-and-import.md) (Chinese).

## Configuring the centre LLM

**There is no one-shot command.** The configuration is `llm.toml`; keys live in the `hub secret` encrypted box; extraction/derivation runs take `--llm-config`:

```bash
cp llm.toml.example "$DATA/llm.toml"          # edit provider / model / secret_ref
recollect-hub secret init --data-dir "$DATA" --box secrets
printf '%s' "$KEY" | recollect-hub secret put --value-stdin --data-dir "$DATA" secrets:deepseek_key
export RECOLLECT_DEVICE_ID=dev-laptop
recollect-hub distill once --pass naming --data-dir "$DATA" --llm-config "$DATA/llm.toml"
```

No key values in `llm.toml`. Model names must match the preset table verbatim; self-hosted gateways use `endpoint`.
Full steps, errors and checks: [LLM configuration](docs/2026-11-09-llm-config.md) (Chinese).

## Docs

- [Agent onboarding](docs/2026-11-08-onboarding.md)
- [Agent self-install and importers](docs/2026-11-09-agent-install-and-import.md)
- [Centre LLM configuration](docs/2026-11-09-llm-config.md)
- [Architecture](docs/2026-10-07-recollect-rust-architecture.md)
- [Releasing](RELEASING.md)

## Boundaries

- Tested here: Windows x86_64 and Linux x86_64 release artifacts, synthetic fixtures, local loopback.
- Not yet validated: arm64 release artifacts on real hardware, long-running `detached` mode, a week of unattended operation.
- No real IPs, usernames, hostnames, credentials or local absolute paths in this file.

## License and thanks

MIT, see [LICENSE](LICENSE). Issues and PRs welcome; no CLA required.

Thanks to Qoder for keeping the work moving, and to qwen3.8-flash for free quota support.

Designed by **lingxinsuixing**; implemented by agents.
