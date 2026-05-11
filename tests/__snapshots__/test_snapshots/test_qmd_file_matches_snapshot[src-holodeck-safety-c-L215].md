---
provider: github
repo: enterprise-d/replicator-firmware
pr_number: 43
thread: "src/holodeck/safety.c:215"
---

# Add holodeck safety interlock for live phaser fire: `src/holodeck/safety.c`:215

<div id="m-e535191e-4f5f-5f98-bcce-5a3457e805ec" data-msg-index="0" class="msg msg--github">

## data-soong (GitHub Review Comment)

*2369-04-15T14:00:02Z · [view on GitHub](https://github.com/enterprise-d/replicator-firmware/pull/43#discussion_r4300302)*

The audible warning loops indefinitely. Counsellor Troi has noted that this may induce undue anxiety in trainees; consider capping the loop at 10 cycles.

</div>
