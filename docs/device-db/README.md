# Device compatibility database (handoff §7.3)

One YAML file per model, named `<brand>-<model>.yaml`. It feeds per-brand quirk handling and,
later, "Tested" labels in the client. Fields:

```yaml
brand: Linn
model: Klimax DSM
generation: "2021 (Organik)"
protocols: [openhome, upnp, airplay]        # from the §7 table
max_pcm: { rate_hz: 384000, bits: 24 }
dsd: { mode: dop, max: dsd256 }             # native | dop | none
gapless: true
volume: { kind: db, min: -80, max: 0, step: 1 }
standby: true
source_switch: true
quirks:
  - room_correction_default_off              # Linn Space Optimisation runs on the device
firmware_tested: "4.102"
tested_on: 2026-10-19
tester: "<pubky or handle>"
notes: ""
```

Values are what we measured, not what the brochure says. Add a file only after a real test.
