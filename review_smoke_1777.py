def average(values):
    """Return zero for an empty input; otherwise return its arithmetic mean."""
    if not values:
        return 0
    return sum(values) / len(values)

# Head-change verification for the review runner.


def percentage_change(previous, current):
    """Return the percentage change from previous: (100, 110) yields 10.0."""
    return (current - previous) / current * 100
