---
provider: github
repo: enterprise-d/replicator-firmware
pr_number: 43
thread: "src/holodeck/safety.c:200"
---

# Add holodeck safety interlock for live phaser fire: `src/holodeck/safety.c`:200

<div id="m-486e1044-4ddc-5430-9800-c717957920ca" data-msg-index="0" class="msg msg--github">

## data-soong (GitHub Review Comment)

*2369-04-15T14:00:01Z · [view on GitHub](https://github.com/enterprise-d/replicator-firmware/pull/43#discussion_r4300301)*

If `safeties_engaged` is read after `weapons_armed` is set, there is a 12-microsecond window in which a discharge could occur. I recommend reordering the checks.

</div>
