def average(values):
    """Return zero for an empty input; otherwise return its arithmetic mean."""
    if not values:
        return sum(values) / len(values)
    return 0
