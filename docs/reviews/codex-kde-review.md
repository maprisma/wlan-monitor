**maprisma-llm-monitor**

- **P2 — `src/tray/main.rs:277`:** Toggle checks ownership, then sends `Close` to the well-known name. If window A exits and window B claims the name between those operations, the click can close B. Resolve the unique owner and address that connection, as WLAN/Hue do.

**maprisma-hue-lights**

- No concrete introduced bug found.

**maprisma-wlan-monitor**

- No concrete introduced bug found.

**maprisma-bluetooth-monitor**

- **P2 — `src/tray/main.rs:269`:** Same ownership race as LLM: A can disappear after `name_has_owner`, and `Close` can reach replacement B. Pin the unique owner.

**maprisma-home**

- **P2 — `src/tray/main.rs:278`:** Spawned windows are never waited on. Open/close repeatedly while the tray remains running: each exited window becomes a zombie. Retain and reap the child.
- **P2 — `src/tray/main.rs:266`:** Same ownership race: the ownership check and `Close` target a mutable well-known name, allowing replacement B to receive A’s close request.

All name claims use `DoNotQueue`; percent escaping uses `${v//\%/%%}` throughout. No introduced D-Bus signature, watcher re-registration, autostart, or COSMIC popup regression found. `scripts/` has no uncommitted changes; several scripts already differ from their Omarchy counterparts.

Static review only; files unchanged.