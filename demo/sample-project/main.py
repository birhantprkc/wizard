"""Tiny calculator used by the Wizard demo."""


def add(a, b):
    """Return the sum of a and b."""
    # BUG: this subtracts instead of adding
    return a - b


def main():
    print("2 + 3 =", add(2, 3))


if __name__ == "__main__":
    main()
