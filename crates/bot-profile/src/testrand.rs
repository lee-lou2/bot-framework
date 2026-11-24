//! 시험 전용 결정적 난수(xorshift). 같은 씨앗이면 같은 입력을 만들어 실패를 다시 재현할 수 있다.
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

/// YAML에서 의미가 있는 조각을 무작위로 이어 붙인 글
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
        "한글",
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
