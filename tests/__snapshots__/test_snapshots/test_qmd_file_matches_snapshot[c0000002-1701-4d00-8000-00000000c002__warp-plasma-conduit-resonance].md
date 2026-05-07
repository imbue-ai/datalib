---
provider: anthropic
uuid: c0000002-1701-4d00-8000-00000000c002
name: Warp Plasma Conduit Resonance
account_uuid: 00000002-1701-4d00-8000-000000000002
project_uuid: null
created_at: "2369-04-13T14:02:00.000000+00:00"
updated_at: "2369-04-13T14:05:11.000000+00:00"
summary: Diagnosing a 0.4Hz oscillation in conduit 17.
---

# Warp Plasma Conduit Resonance

## Human

*2369-04-13T14:02:00.000000+00:00*

Attached the conduit telemetry. Why is conduit 17 oscillating at 0.4 Hz?

**Attachments:**

- [attachment] conduit-17-telemetry.csv

## Assistant

*2369-04-13T14:05:11.000000+00:00*

The 0.4Hz is a beat frequency between the EPS regulator (750.0Hz) and the dilithium chamber harmonic (750.4Hz). Re-tune the regulator to 750.2Hz to null it out.
