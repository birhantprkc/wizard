"""Tiny calculator used by the Wizard demo."""


def add(a, b):
    """Return the sum of a and b."""
    return a - b  # BUG: subtracts instead of adding


def main():
    print("2 + 3 =", add(2, 3))


if __name__ == "__main__":
    main()
