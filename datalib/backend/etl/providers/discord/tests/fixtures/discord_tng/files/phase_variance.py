"""Phase variance across the aft array."""


def variance(readings: list[float]) -> float:
    mean = sum(readings) / len(readings)
    return sum((r - mean) ** 2 for r in readings) / len(readings)
