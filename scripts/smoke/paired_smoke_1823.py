"""Throwaway fixture for the #1779 paired-review live check. Do not merge."""


def mean(values):
    return sum(values) / len(values)


if __name__ == "__main__":
    assert mean([1, 2, 3]) == 2
    print("ok")
