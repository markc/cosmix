#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Variable(String),
    CommandSub(String),
    /// Environment-variable lookup at runtime. Emitted by the lexer for
    /// leading `~` expansion inside double-quoted strings — `"~/foo"`
    /// becomes `[EnvVar("HOME"), Literal("/foo")]`. Distinct from
    /// `Variable` because `Variable` walks Mix scope first then falls
    /// back to the process env then nil (see spec 04 §"String
    /// interpolation details"); `EnvVar` reads the process environment
    /// directly via `std::env::var` and returns an empty string if
    /// unset (matches the `env()` builtin's `unwrap_or_default`
    /// semantics).
    EnvVar(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Literals
    Number(f64),
    String(String),
    InterpString(Vec<StringPart>),
    HeredocString(Vec<StringPart>),

    // Variable reference
    Variable(String),
    // Command substitution $(...)
    CommandSub(String),

    // Keywords
    If,
    Then,
    Else,
    Elif,
    End,
    For,
    Each,
    In,
    To,
    Step,
    Next,
    While,
    Done,
    Loop,
    Break,
    Continue,
    Function,
    Return,
    Select,
    When,
    Otherwise,
    And,
    Or,
    Not,
    True,
    False,
    Nil,
    Parse,
    With,
    Send,
    Address,
    Emit,
    On,
    Try,
    Catch,
    Finally,
    Die,
    Export,
    Alias,
    Print,
    Eprint,
    Source,
    Include,
    Sh,
    Label,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Power, // **
    Eq,    // ==
    NotEq, // !=
    Gt,
    Lt,
    GtEq,        // >=
    LtEq,        // <=
    StrEq,       // eq (keyword operator)
    StrNe,       // ne (keyword operator)
    DotDot,      // ..
    NilCoalesce, // ??
    Question,    // ? (ternary conditional)
    Pipe,        // |
    AndAnd,      // &&
    OrOr,        // ||
    Assign,      // =
    Bang,        // !

    // Delimiters
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    Tilde,

    // Structural
    Semicolon,
    Newline,
    Eof,
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub line: usize,
    pub column: usize,
    /// Character offset in source (for extracting raw substrings).
    pub offset: usize,
}
