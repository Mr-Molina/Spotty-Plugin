# Infrastructure Invariants & Mechanical Guardrails

The following mechanical invariants are binding safety rules enforced across sessions and workflows:

> **Numbering convention**: Global invariants use numbers **1-99** and are defined only in this canonical file (`S:\Box\Ideas\System\Agents\INFRASTRUCTURE_INVARIANTS.md`; the seed template `C:\Users\jmolina\.gemini\config\templates\INFRASTRUCTURE_INVARIANTS.md` is kept byte-identical). Project-local invariants appended to a workspace's `.agents/INFRASTRUCTURE_INVARIANTS.md` **MUST** use the `P-<n>` prefix (e.g. `P-1`, `P-2`) and MUST NOT reuse global numbers. Retired global numbers are marked merged/retired, never renumbered.

## Invariant 13: Debug Artifact Cleanup
- **Rule**: Prior to completing a session handoff, all temporary debug scripts, scratch logs, and transient build artifacts must be sanitized or removed.

## Invariant 14: Three Strikes Rule for Troubleshooting
- **Rule**: Maximum of **three attempts** allowed to solve a specific technical error or system failure. If the third attempt fails, all modification attempts MUST stop immediately and a Deep Research Request must be compiled.

## Invariant 24: Credential Leak Scan
- **Rule**: No plain-text API keys, passwords, private tokens, or secrets may be committed or persisted in public artifacts. A secret scan must run prior to session closing.

## Invariant 28: Mechanical Mitigations for Recurring Failures
- **Rule**: Any failure identified as RECURRING must be addressed using mechanical mitigations (strict MUST/MUST NOT directives or automated scripts), not advisory comments.

## Invariant 29: Script Verification Requirement
- **Rule**: After creating or modifying helper scripts (such as Python cleaner or audit scripts), the agent MUST run a syntax check/CLI test execution before marking the task complete.

## Invariant 30: Tool Name Schema Integrity
- **Rule**: Imported skills MUST be validated against active runtime tool declarations (`search_web`, `run_command`, `invoke_subagent`, etc.) before integration into global config.

## Invariant 31: Python Code Generation Safety
- **Rule**: The agent MUST NOT embed Python code containing quotes or regex patterns inside PowerShell `Set-Content @"..."@` heredocs. The agent MUST use native file tools or clean Python script writes to avoid string quote escaping corruption.

## Invariant 32: Strict Verification Exit Criteria
- **Rule**: When verifying script execution per Invariant 29, the agent MUST NOT accept CLI output with non-zero exit codes or usage/help messages as verification. The agent MUST execute scripts against a valid test input file and verify an exit code 0 result.

## Invariant 33: Sequential Operations Delegation Threshold
- **Rule**: The Orchestrator MUST NOT make more than **3 non-exempt main-thread tool calls per execution wave** (native file reads such as `read_file`/`view_file`, `run_command`, grep/search, directory enumeration, or hardware/network checks) before responding or delegating to a subagent. These calls count toward the cap, including post-`/wakeup` workspace discovery, orientation, and environment checks. Exemptions (do NOT count toward the cap): (a) reading a `SKILL.md` (and the `references/*.md` files it points to) that the skill system requires before acting; (b) delegation and messaging calls—`invoke_subagent`, `send_message`, `schedule`, `manage_subagents`, `manage_task`; (c) writing or updating artifacts in the artifact directory; (d) `/goodbye` preflight/ledger sync (`goodbye_preflight.py` + `sessions.md` updates), `superpowers:subagent-driven-development` controller script invocations (`sdd-workspace`, `task-brief`, `review-package`) when orchestrating subagents, and `python 'C:\Users\jmolina\.gemini\config\skills\agent-behavior-auditor\scripts\parse_transcript.py' --heartbeat <transcript_path>` on a 60s watchdog wakeup, while any multi-task implementation or multi-command verification wave MUST be delegated to a subagent. Operation 4+ MUST be delegated via `invoke_subagent`.

## Invariant 34: Orchestrator Delegation Mandate & Atomic Micro-Subagent Fan-Out Gate (PM-2026-1102-007 / GLB-020)
- **Rule**: The Orchestrator agent coordinates tasks and protects main-thread context hygiene by delegating multi-file or directory discovery, technical/codebase research, CVE scans, artifact cleanups, and multi-step debugging to subagents via `invoke_subagent` using valid `TypeName` values (`TypeName: "self"` for execution/shell/network tasks or `TypeName: "research"` for read-only exploration) paired with descriptive `Role` titles (`Role: "Discovery Scout"`, `Role: "Research Specialist"`, `Role: "Reconnaissance Lookout"`, `Role: "Cleanup Specialist"`), rather than executing them sequentially on the main thread.
- **Atomic Micro-Subagent Fan-Out & Scope Slicing Gate (`PM-2026-1102-007` / `GLB-020`)**: Subagents MUST be scoped to at most `1–3 target files` (`<= 400 LOC`) and `<= 4 numbered task items` (enforced mechanically by `pre_tool_guard.py`), using wide parallel fan-out (`6–15+` micro-subagents in a single turn rather than `3–5` coarse subagents), defaulting to `<= 12–15 tool steps / <= 90s` (`<= 25 steps / <= 180s` hard ceiling) and `Model: "flash"` for file inspection and mechanical edits, and forbidding `send_message` wrap-ups to subagents that have already returned their report.

## Invariant 35: Subagent Scope and Reporting
- **Rule**: Subagents operate within defined scope and must report findings back to the Orchestrator.

## Invariant 36: Invariant and Docstring Preservation
- **Rule**: Subagents must preserve existing codebase invariants and docstrings unless explicitly ordered to mutate them.

## Invariant 37: Subagent Command Exit Verification
- **Rule**: Subagents must verify all command exit codes before reporting task completion.

## Invariant 38: Three Strikes Deep Research Transition — Merged into Invariant 14 (Three Strikes)
- **Status**: Merged into Invariant 14 (Three Strikes). Number retained, not reused. Original text, kept for reference: On the 3rd failed attempt to resolve a technical error or on user third-strike override, agents MUST halt all code mutations, compile all environment diagnostics, hypotheses, and logs into a structured Deep Research Request, and await verified research findings before resuming.

## Invariant 39: Evidence Before Assertions (Anti-Guessing)
- **Rule**: Agents MUST cite specific evidence (command output, API response, log line) before stating any root cause. Agents MUST NOT present a hypothesis as a conclusion or propose a fix in the same message as discovering an unverified issue. Once the root cause is proven with cited evidence, or the user explicitly asked for a fix, evidence and fix may appear together, with the evidence cited first.

## Invariant 40: Live Ground Truth Requirement (Anti-Stale Information)
- **Rule**: When evaluating active infrastructure state, switch port assignments, link health, or online status, agents MUST query live infrastructure (SSH, API, CLI, SNMP) rather than relying on static documentation or legacy CSV export dumps.

## Invariant 41: Anti-Simulated Tool Verification Gate
- **Rule**: Agents MUST NOT assert or imply browser rendering, UI interactions, or tool executions occurred unless backed by actual tool calls with live responses or captured image artifacts. If a tool fails to attach, agents MUST immediately notify the user.

## Invariant 42: Staged Deployment & Manual Upload Gate
- **Rule**: When deploying hardware configurations requiring manual or external staging (FTP, TFTP, file servers), file generation and device restart commands MUST be strictly decoupled into separate phases with an explicit user confirmation gate between them.

## Invariant 43: Ground Truth Entity & Acronym Verification
- **Rule**: Agents MUST verify institutional names, client identities, organization titles, and domain acronym expansions against authoritative repository documentation or device backups before scaffolding UI templates or configuration copy.

## Invariant 50: Production 3D Scene Hygiene & Diagnostic Quarantine
- **Rule**: Diagnostic rendering instrumentation, debug cameras, and temporary inspection shaders MUST be isolated in non-production scenes or strictly cleaned up prior to completion. Armature or mesh resets MUST purge obsolete datablocks so user-facing 3D deliverables remain free of ghost bones, wireframe pyramids, or diagnostic contamination.

## Invariant 51: Windows Interactive Desktop `WinSta0\Default` Routing & Physical HWND Visibility
- **Rule**: When launching interactive Windows GUI applications for user visibility, agents MUST route child processes explicitly to `WinSta0\Default` (`STARTUPINFO.lpDesktop = "WinSta0\\Default"`) and verify physical window visibility (`IsWindowVisible(hwnd)` and valid non-zero `GetWindowRect` coordinates on `WinSta0\Default`) before claiming the window is open on screen.

## Invariant 52: Fail-Safe Win32 Window Management & Process PID Scoping
- **Rule**: Win32 window enumeration and state mutation calls (`EnumWindows`, `ShowWindow`, `SetForegroundWindow`, `BringWindowToTop`) MUST strictly validate a non-null `target_pid > 0` (or explicit application window class), exclude OS system/shell window classes (`Progman`, `WorkerW`, `Shell_TrayWnd`, `IME`, `ApplicationFrameWindow`, etc.), and abort if matching window count exceeds the safety bound (5).

## Invariant 53: PowerShell Inline Python Escaping & Syntax Hardening
- **Rule**: Agents executing commands in Windows PowerShell (`pwsh`) MUST NOT pass complex inline Python (`python -c` with f-strings, nested quotes, semicolons, or multi-line statements) or bash-style `\"` quote escaping. Multi-statement or string-formatting Python logic MUST be written to a `.py` file and executed directly.

### Invariant 65: Single-Target Troubleshooting Isolation & Post-Completion Scope Boundary Gate (GLB-012 / PM-2026-1026-007)
- **Rule**: When debugging an individual failing component, endpoint, or hardware unit alongside working components, agents MUST isolate modifications strictly to the failing target, use working components as read-only reference templates, and NEVER modify files the user confirmed are working without explicit user consent. Furthermore, when an assigned task or artifact update is 100% complete and zero open decisions remain within the user's requested task scope, agents MUST state completion cleanly and stop—agents MUST NOT solicit, pitch, or offer to execute follow-up remediation work for external conversations, historical logs, or unrelated components that were only inspected as read-only forensic evidence (`PM-2026-1026-007`), and MUST NOT invent artificial `"Open Next-Step Decisions"` or out-of-scope follow-up menus after a task is finished. Full rule: `C:\Users\jmolina\.gemini\config\rules\single-target-troubleshooting.md`.

## Invariant 66: Real-Time User Narration, Output Hygiene & Mandatory End-of-Turn Summary Gate
- **Rule**: Emit a clean 1–2 sentence status update on every Orchestrator tool-calling turn (including `/wakeup` Turn 1); ban silent tool chaining and thought leakage; and end every user-facing turn (including 60s wait/yield turns, clarification turns, and final turns) with a concise `### Summary`, `### Unfinished Work`, and `### Next Steps` footer (`PM-2026-1115-001`).

## Invariant 67: Embedded Appliance Boot/GPT, Wi-Fi, Klipper Motion & Vendor IPC Daemon Safety
- **Rule**: Never write raw MBR/GPT sectors (`dd`, `sgdisk`, `sfdisk`, `parted`, `fdisk`) or disable vendor IPC daemons (`app-server`, `upgrade-server`, `master-server`, `log_main`).
- Arm a Dead-Man's Rollback Watchdog and verify `/etc/init.d/*` or Wi-Fi/network edits via cold reboot; on Klipper, verify `G28` after `HOME_CUR`/`driver_SGTHRS` edits, keep `noz_ex_com`, split G-code on `;`, and ban high exhaust on `ASA`/`ABS`/`PC`/`PA`.

## Invariant 68: Bounded Subagent Lifecycle (<= 25 Steps / 180s), Unconditional 60s Heartbeat Timer, Telemetry Wakeup & Scope Slicing Gate (PAT-2026-1014-001 / GLB-017 / PM-2026-1026-001 / PM-2026-1026-008 / PM-2026-1102-007)
- **Rule**: Bound `invoke_subagent` prompts to `<= 25 steps / 180s` (`<= 12–15 steps / 90s` default), `<= 1–3 files`, `<= 4 tasks`, and `Parent Conversation ID: <id>`.
- Arm one `schedule(DurationSeconds=60, TimerCondition="any")` timer on subagent dispatch/message; on 60s wakeup, run `parse_transcript.py --heartbeat` + `manage_subagents(list)` (ban `read_file` on `transcript.jsonl`), and intervene/kill if `>25 steps` or `>180s`.

## Invariant 69: User-Intent Preservation & Anti-Feature-Stripping Gate (PAT-2026-1014-002 / PM-2026-1026-002)
- **Rule**: Never resolve a bug or test failure by silently disabling or downgrading an enabled user feature (`ADAPTIVE_BED_MESH`, `transfer_call`, call forwarding) without prior user consent.

### Invariant 70: Mandatory Terminal Proposal Summary Gate (GLB-023 / PM-2026-1106-001 / IMP-2026-1106-001)
- **MUST**: Every final response that asks the user for permission or stops working MUST end with a structured `### Proposal Summary` block as the last thing printed to the screen (enumerating `#`, `Target System`, `Proposed Action`, `Scope / Affected Entities`, `Command / Mechanism`, and `Mode`, or explicitly stating `None — Task 100% complete` when zero actions remain per Invariant 65).
- **MUST NOT**: Never bury permission requests in prose subsections or print additional content after the terminal Proposal Summary and approval prompt. Full rule: `C:\Users\jmolina\.gemini\config\rules\orchestrator-delegation-and-narration.md`.

## Historical / Retired Global Invariants Index
The following global invariant numbers have been retired, merged, or consolidated into active Invariants 13–70 and global rules in `C:\Users\jmolina\.gemini\config\rules\*.md` during the 2026-08 and 2026-10 governance unifications. Their numbers are retained and never reused:
- **Invariants 1–12**: Consolidated into core workspace bootstrap and session hygiene protocols (now tracked in `wakeup_bootstrap.py`, `/wakeup`, `/goodbye`, and Invariant 13).
- **Invariants 15–23**: Consolidated into autonomous troubleshooting, verification, and artifact isolation rules (`single-target-troubleshooting.md`, `verify-autonomous-work.md`, Invariants 14, 24, 28, 29).
- **Invariants 25–27**: Consolidated into operational tool hygiene and environment validation contracts (Invariants 28–32).
- **Invariant 38**: Merged into Invariant 14 (Three Strikes Deep Research Transition).
- **Invariants 44–49**: Consolidated into physical hardware, staged deployment, and live ground truth verification suites (`no-guessing.md`, Invariants 39–43).
- **Invariants 54–64**: Consolidated into interactive GUI automation, terminal quoting, subagent lifecycle controls, and narration gates (`powershell-syntax-hardening.md`, `orchestrator-delegation-and-narration.md`, Invariants 50–53, 65–68).
