---
provider: gitlab
project: enterprise-d/holodeck
mr_iid: 18
thread: "tests/safety.rs:42"
---

# Holodeck safety interlock regression test: `tests/safety.rs`:42

<div id="m-0e6485d8-fa08-53f0-954a-68e905de6352" data-msg-index="0" class="msg msg--gitlab">

## data-soong

*2369-04-15T14:00:00.000Z · [view on GitLab](https://gitlab.com/enterprise-d/holodeck/-/merge_requests/18#note_1700018001)*

The assertion only checks the safety flag *before* arming. We should also assert that arming the weapon while safeties are engaged returns Err(SafetyEngaged).

</div>

<div id="m-e675546a-64c5-5da7-97b3-3384b96d3427" data-msg-index="1" class="msg msg--gitlab">

## geordi-laforge

*2369-04-15T14:10:00.000Z · [view on GitLab](https://gitlab.com/enterprise-d/holodeck/-/merge_requests/18#note_1700018001)*

Agreed — I can wire that into the simulator harness if Data writes the assertion.

</div>

<div id="m-a5a87972-912b-5c13-a00d-fca57102b632" data-msg-index="2" class="msg msg--gitlab">

## jlpicard

*2369-04-15T14:20:00.000Z · [view on GitLab](https://gitlab.com/enterprise-d/holodeck/-/merge_requests/18#note_1700018001)*

Concur. Make it so.

</div>
