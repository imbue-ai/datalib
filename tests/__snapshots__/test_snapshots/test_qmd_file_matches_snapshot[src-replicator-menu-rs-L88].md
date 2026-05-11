---
provider: gitlab
project: enterprise-d/holodeck
mr_iid: 17
thread: "src/replicator/menu.rs:88"
---

# Add Earl Grey, hot to replicator menu: `src/replicator/menu.rs`:88

<div id="m-3fd3375d-d29e-5222-afda-85db13d3c654" data-msg-index="0" class="msg msg--gitlab">

## wtriker

*2369-04-15T10:00:00.000Z · [view on GitLab](https://gitlab.com/enterprise-d/holodeck/-/merge_requests/17#note_1700017001)*

Use `enum Tea { EarlGrey, Chamomile, ... }` here rather than a string match — it'll catch typos at compile time.

</div>

<div id="m-f1de1bb5-cfd5-5ee4-aecb-e90fa23d8c8c" data-msg-index="1" class="msg msg--gitlab">

## jlpicard

*2369-04-15T10:05:00.000Z · [view on GitLab](https://gitlab.com/enterprise-d/holodeck/-/merge_requests/17#note_1700017001)*

Adopted. Make it so.

</div>
