# Candidate feature: toggle the front-panel display

Proposed 2026-08-02, alongside the alarm toggle it would sit next to. Status:
**candidate registers identified, not yet confirmed against the button.** Do
not build until the experiment below has named the register.

## The idea

The tray menu already toggles the audible alarm, which is a write to a register
(`AudibleAlarmControl`, reports 24/120) that the vendor otherwise makes you load
a web app to reach. The unit's LCD has the same shape of problem: it is
controlled by a button on the UPS's face, under the desk, and if that button is
backed by a register the tray could cycle the display the same way — off for a
dark room, on when you actually want the numbers.

## What is measured so far

There is no standard display usage in the HID Power Device page, so if this
exists it is vendor territory. The full caps walk (`--probe`) leaves exactly
two unnamed APC registers with a toggle's shape:

| Report | Usage | Range | Read 2026-08-02 |
|---|---|---|---|
| 53 | `FF86:61` | u8, 0..2 | `1` |
| 121 | `FF86:72` | u8, 0..1 | `0` |

Report 53 is the promising one: three states, and the display button cycles
three states (on / dim / off), the way the alarm's register is three states
(disabled / enabled / muted). Report 121 is a plain boolean and the only other
candidate. Every other unnamed vendor register is counter-, voltage- or
constant-shaped (`FF86:26` reads a fixed 156).

The baseline of `53 = 1` is consistent with a display sitting in its middle
state, and is also the value a safe first write would use.

## The experiment that settles it

Five minutes at the machine, passive first:

1. `jdups --read 53,121` for a baseline.
2. Press the display button on the unit once; read again.
3. Press again; read again. If report 53 tracks the cycling, it is the display.
4. Only then, the safe first write: `jdups --set 53 <a value already observed>`.
   Writing a value the register already held proves the whole path -- write,
   settle, readback -- while changing nothing, the same trick as `-1` into an
   idle countdown.
5. Toggle it for real and watch the LCD. Note which value maps to which state.

If neither register moves at step 2, the button is firmware-local, the display
is not exposed, and this document becomes the record of why the feature does
not exist.

**Do not write either register before the button experiment names it.** They
are unnamed vendor usages on a live UPS; the range makes 53 display-shaped, but
writing blind to see what happens is the exact class of assumption this
project's history keeps punishing. Observing the button first turns the write
into a confirmation rather than a probe.

## If it pans out: what gets built

The alarm toggle, again, almost line for line:

- A menu item on the **device thread** (`tray/device.rs`), never the UI thread:
  the request is left in an atomic, the thread that owns the handle performs
  the write with the settle loop, and the readback lands in the snapshot -- the
  menu shows what the UPS holds, not what was asked.
- Placed with the alarm toggle, below the separator that keeps write items away
  from the read-and-copy rows.
- No confirmation toast, per the alarm decision: user-initiated, instant, and
  the menu state is the feedback.
- `--set 53` already works from the CLI once the meaning is known; the guard
  refuses only the countdown registers (21, 64, 65, 66), which this is not.

Also worth capturing while at it: whether the register's value survives the
UPS losing mains entirely, and whether PowerChute's own UI exposes the display
anywhere (it does not appear to for this model, which would make this a small
genuine win over the vendor software, same as the alarm).
