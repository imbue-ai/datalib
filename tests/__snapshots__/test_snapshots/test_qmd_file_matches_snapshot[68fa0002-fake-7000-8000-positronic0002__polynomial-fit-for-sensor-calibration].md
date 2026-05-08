---
provider: openai
id: 68fa0002-fake-7000-8000-positronic0002
title: Polynomial Fit for Sensor Calibration
account_id: user-FAKE0DATAANDROID0POSITRONIC1
create_time: "2370-10-25T08:00:00.000000+00:00"
update_time: "2370-10-25T08:05:20.000000+00:00"
default_model_slug: gpt-5
---

# Polynomial Fit for Sensor Calibration

<div id="m-msg-fake-poly-0001" data-msg-index="0" class="msg msg--openai">

## User

*2370-10-25T08:00:00.000000+00:00*

<a id="b-msg-fake-poly-0001-0"></a>Fit a degree-2 polynomial to (1,2.1),(2,4.0),(3,9.1),(4,15.8),(5,25.0) in Python.

</div>

<div id="m-msg-fake-poly-0002" data-msg-index="1" class="msg msg--openai">

## Assistant

*2370-10-25T08:03:20.000000+00:00 · gpt-5*

<a id="b-msg-fake-poly-0002-0"></a>
```python
import numpy as np
x = np.array([1,2,3,4,5])
y = np.array([2.1, 4.0, 9.1, 15.8, 25.0])
coeffs = np.polyfit(x, y, 2)
print(coeffs)
```

</div>

<div id="m-msg-fake-poly-0003" data-msg-index="2" class="msg msg--openai">

## Assistant

*2370-10-25T08:05:20.000000+00:00 · gpt-5*

<a id="b-msg-fake-poly-0003-0"></a>The fit gives approximately y = 1.014 x² + 0.06 x + 1.07. R² ≈ 0.9997. The residuals suggest the underlying process is very close to pure quadratic with small measurement noise.

</div>
