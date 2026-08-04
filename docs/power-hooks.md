# Power-event hooks: run a command when the power state changes

Planned and built 2026-08-03: three keys in `jdups.conf`, executed by the tray
(`tray/hooks.rs`), the state fold pinned by tests. What remains is using it --
uncomment the keys, restart the tray, and the next plug-pull is a lighting
demo. The motivating use is
lighting -- jdrgb sets the room amber on battery, red while a shutdown is
pending, back to normal when mains returns -- but the feature is deliberately
not about jdrgb. "Run this script when the power goes out" is the oldest
feature in UPS software (apcupsd's `onbattery`/`offbattery`, NUT's
`upssched`); jdups gets the general mechanism and one user's config makes it
about lights.

## The shape

Three optional keys in `jdups.conf`, empty or absent meaning off:

```
# on_battery_cmd =
# on_pending_cmd =
# on_mains_cmd =
```

Each is a full command line, run via `cmd /C` with no window, fire-and-forget.
The motivating case, as actually deployed (jdrgb's `--stash` saves the colour
on the way into an alert; `restore` puts it back):

```
on_battery_cmd = C:\bin\jdrgb.exe amber --all --stash
on_pending_cmd = C:\bin\jdrgb.exe red --all --stash
on_mains_cmd   = C:\bin\jdrgb.exe restore --all
```

(The "restore whatever it was before" version wanted jdrgb to remember its own
last setting, and as of 2026-08-03 it does: `--stash` saves the current colour
on the way into an alert and a `restore` subcommand puts it back. So
`on_battery_cmd` stashes-and-sets, `on_mains_cmd` restores, and jdups never
learned a thing about lighting -- each tool grew its own half and the config
file is the only place they meet. In use, live, on the machine this was built
for.)

## The decisions that matter

- **The tray runs the hooks, never the agent.** Two hard reasons. The agent is
  SYSTEM, and a SYSTEM process executing a command line from configuration is
  an escalation the moment the named binary lives somewhere user-writable,
  which `C:\bin` is. And the agent is in session 0, where a lighting tool may
  not even reach the hardware. The tray is the user's session and the user's
  privileges, so nothing crosses a boundary: on a machine-wide install the
  conf is admin-writable anyway, and on a per-user install the conf, the tray
  and the commands are all the same user. Cosmetics live in the cosmetic
  process; if the tray is dead the lights are wrong and the shutdown still
  works.
- **State-driven, not event-driven.** The hook layer computes one of three
  states -- `Mains`, `OnBattery`, `Pending` -- and runs a command only when the
  state *changes*. That gets every path right without special cases: a pending
  shutdown cancelled by mains return runs `on_mains_cmd`; one cancelled while
  still on battery (a user-present wake retraction) falls back to
  `on_battery_cmd`; flapping cannot re-run a command for a state the lights
  are already in.
- **`Pending` wins over everything, dry run included.** If the agent has
  announced a countdown, the room goes red whatever the tray's own device view
  says, and a dry-run countdown lights up too -- which makes the plug-pull
  test a lighting demo for free.
- **Unknown holds.** Losing sight of the UPS is not a power state; the lights
  keep whatever state was last known, symmetrical with how the notifications
  refuse to announce recoveries they never saw.
- **Startup fires battery and pending, never mains.** The first known state
  after the tray starts runs its hook only if it is `OnBattery` or `Pending`
  (starting mid-outage should light up), but never `on_mains_cmd`: every
  logon resetting the room's lighting to "normal" would be the tray stomping
  on whatever the user had actually set. Same asymmetry the startup toasts
  have.
- **The agent parses the keys and ignores them.** `jdups.conf` treats unknown
  keys as fatal on purpose, so the keys are known to `config.rs` and simply
  unused by the agent; its startup log records them like everything else.
- **Failures are silent.** Fire-and-forget spawn, no window, no retry, no
  log: there is nowhere user-visible to report from the tray, and a lighting
  command that fails is a cosmetic problem in the cosmetic process. Test by
  toggling the state, not by reading logs.

## What the hooks cost the UPS, and the rule that follows

A hook runs a program *while the power is failing*, which turns out to matter
more than it sounds. On 2026-08-03 `jdrgb restore` at mains-return wedged the
UPS's USB interface twice, because `HidApi::new()` enumerates the whole HID bus
and Windows enumeration opens every device to read its string descriptors --
so a command about case lights was interrogating the UPS at the worst possible
instant. jdrgb was fixed to look at one VID/PID (see status.md), but the
general lesson belongs here:

**A hook command should touch nothing this program depends on.** Anything that
scans USB, HID, or the power subsystem is being invoked at exactly the moment
that subsystem is least able to take it. Lighting, sound, a notification, a
log line: fine. A device inventory: not fine, however innocent it looks in
isolation.

## Known limitations, accepted

- The parser strips `#` comments, so a command line cannot contain a literal
  `#`. Nothing this is for needs one.
- The UPS's periodic self-test transfers to battery for a few seconds and the
  tray cannot tell a test from a brownout (report 33 is not read here), so the
  monthly self-test buys a short amber blip. Treat it as a scheduled
  demonstration that the pipeline works.
- Commands run at power-state cadence, so at most a handful per day. No
  debounce beyond the state change itself is needed; the tray's power state is
  already smoothed and the pending flag already latched.

## Build order

1. `config.rs`: three `Option<String>` fields, parse arms (empty value means
   unset, so the template stays inert when uncommented), `describe()` lines,
   template text, tests alongside the existing parser tests.
2. `tray/hooks.rs`: the state enum, the change-detecting fold, and the
   spawn-with-no-window helper. The fold is pure and tested; the spawn is not.
3. Wiring: the tray loads `Settings` once at startup (a config the agent
   refuses does not kill the tray; the hooks just stay off), and the hook fold
   runs wherever power or pending changes land -- the end of `refresh` and of
   `set_pending`.
4. Docs: README one-liner in the tray bullet, template regenerated, this file
   updated to "built".
