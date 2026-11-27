//! Deterministic xorshift PRNG and YAML fuzz token generator for tests.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

pub fn yaml_soup(rng: &mut Rng) -> String {
    const TOKENS: &[&str] = &[
        "a",
        "b",
        "key",
        "1",
        "-7",
        "true",
        "null",
        "~",
        ":",
        ": ",
        "- ",
        "? ",
        "[",
        "]",
        "{",
        "}",
        ",",
        "&x ",
        "*x ",
        "!t ",
        "!!str ",
        "|",
        ">",
        "'",
        "\"",
        "#",
        "%YAML 1.2\n",
        "@",
        "---\n",
        "...\n",
        "\n",
        "  ",
        "\t",
        " ",
        "\\",
        "<<",
        ".inf",
        ".nan",
        "0x1F",
        "0o17",
        "1e999",
        "\u{202e}",
        "\\u0000",
        "€★",
        "[[",
        "]]",
        "{{",
        "}}",
        "- - ",
        ": - ",
        "? - ",
        "|\n  x\n",
        ">-\n  y\n",
    ];
    let count = 1 + rng.below(40);
    (0..count).map(|_| *rng.pick(TOKENS)).collect()
}
