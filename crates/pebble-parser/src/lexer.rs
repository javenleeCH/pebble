//! 词法分析：UTF-8、`//` 行注释、双引号字符串、i64/f64 字面量。
//! `|`（Lambda 已废除）与 `^`（不存在幂运算）是非法字符，给出针对性提示。

use pebble_diagnostic::{Code, DiagBag, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    /// 整数字面量；超出 i64 范围时词法层保留 i128 值，语义层报错。
    Int(i128),
    Float(f64),
    Str(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Dot,
    Eq,     // ==
    Assign, // =
    Colon,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
    Not,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub fn lex(source: &str, diags: &mut DiagBag) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes: Vec<char> = source.chars().collect();
    // 字节偏移 → 字符索引映射：直接以字符索引记录，Span 语义按“字符偏移”处理
    //（诊断渲染同样按 char 计数，二者一致）。
    let n = bytes.len();
    let mut i = 0usize;

    macro_rules! span {
        ($s:expr, $e:expr) => {
            Span::new($s, $e)
        };
    }

    while i < n {
        let c = bytes[i];
        // 空白
        if c == ' ' || c == '\t' || c == '\r' {
            i += 1;
            continue;
        }
        if c == '\n' {
            i += 1;
            continue;
        }
        // 注释
        if c == '/' && i + 1 < n && bytes[i + 1] == '/' {
            while i < n && bytes[i] != '\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        // 标识符：MVP 仅 ASCII（02-lexical）
        if c.is_ascii_alphabetic() || c == '_' {
            i += 1;
            while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '_') {
                i += 1;
            }
            let name: String = bytes[start..i].iter().collect();
            out.push(Token {
                tok: Tok::Ident(name),
                span: span!(start, i),
            });
            continue;
        }
        // 数字
        if c.is_ascii_digit() {
            while i < n && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < n && bytes[i] == '.' {
                if i + 1 < n && bytes[i + 1].is_ascii_digit() {
                    i += 1;
                    while i < n && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    let text: String = bytes[start..i].iter().collect();
                    let value: f64 = text.parse().unwrap_or(f64::INFINITY);
                    out.push(Token {
                        tok: Tok::Float(value),
                        span: span!(start, i),
                    });
                } else {
                    // `1.` 形式
                    diags.error(
                        Code::LexBadNumber,
                        format!(
                            "数字字面量 `{}` 后的小数点缺少数字",
                            bytes[start..i + 1].iter().collect::<String>()
                        ),
                        span!(start, i + 1),
                    );
                    i += 1;
                }
            } else {
                let text: String = bytes[start..i].iter().collect();
                match text.parse::<i128>() {
                    Ok(v) => out.push(Token {
                        tok: Tok::Int(v),
                        span: span!(start, i),
                    }),
                    Err(_) => {
                        diags.error(
                            Code::LexBadNumber,
                            format!("数字字面量 `{}` 过大，无法解析", text),
                            span!(start, i),
                        );
                    }
                }
            }
            continue;
        }
        // `.` 开头且后跟数字 → `.5` 形式非法
        if c == '.' && i + 1 < n && bytes[i + 1].is_ascii_digit() {
            diags.error(
                Code::LexBadNumber,
                format!("数字字面量 `.{}…` 缺少整数部分", bytes[i + 1]),
                span!(start, i + 2),
            );
            i += 2;
            continue;
        }
        // 字符串
        if c == '"' {
            i += 1;
            let mut value = String::new();
            let mut closed = false;
            while i < n {
                let ch = bytes[i];
                if ch == '"' {
                    closed = true;
                    i += 1;
                    break;
                }
                if ch == '\n' {
                    break;
                }
                if ch == '\\' {
                    if i + 1 < n {
                        let esc = bytes[i + 1];
                        let replacement = match esc {
                            '"' => Some('"'),
                            '\\' => Some('\\'),
                            'n' => Some('\n'),
                            't' => Some('\t'),
                            _ => None,
                        };
                        match replacement {
                            Some(r) => {
                                value.push(r);
                                i += 2;
                            }
                            None => {
                                diags.error(
                                    Code::LexBadEscape,
                                    format!("非法转义序列 `\\{}`", esc),
                                    span!(i, i + 2),
                                );
                                i += 2;
                            }
                        }
                    } else {
                        break;
                    }
                } else {
                    value.push(ch);
                    i += 1;
                }
            }
            if !closed {
                diags.error(
                    Code::LexUnclosedString,
                    "字符串未闭合（行尾未遇到结束引号）",
                    span!(start, i),
                );
            } else {
                out.push(Token {
                    tok: Tok::Str(value),
                    span: span!(start, i),
                });
            }
            continue;
        }
        // 运算符与分隔符（最长匹配优先）
        let two: String = bytes[i..(i + 2).min(n)].iter().collect();
        let matched_two = match two.as_str() {
            "==" => Some(Tok::Eq),
            "!=" => Some(Tok::Neq),
            ">=" => Some(Tok::Gte),
            "<=" => Some(Tok::Lte),
            "&&" => Some(Tok::AndAnd),
            "||" => Some(Tok::OrOr),
            _ => None,
        };
        if let Some(t) = matched_two {
            out.push(Token {
                tok: t,
                span: span!(start, i + 2),
            });
            i += 2;
            continue;
        }
        let single = match c {
            '(' => Some(Tok::LParen),
            ')' => Some(Tok::RParen),
            '{' => Some(Tok::LBrace),
            '}' => Some(Tok::RBrace),
            ',' => Some(Tok::Comma),
            '.' => Some(Tok::Dot),
            '=' => Some(Tok::Assign),
            ':' => Some(Tok::Colon),
            '>' => Some(Tok::Gt),
            '<' => Some(Tok::Lt),
            '+' => Some(Tok::Plus),
            '-' => Some(Tok::Minus),
            '*' => Some(Tok::Star),
            '/' => Some(Tok::Slash),
            '%' => Some(Tok::Percent),
            '!' => Some(Tok::Not),
            _ => None,
        };
        if let Some(t) = single {
            out.push(Token {
                tok: t,
                span: span!(start, i + 1),
            });
            i += 1;
            continue;
        }
        // 非法字符（含已废除的 `|`、不存在的 `^`）
        let hint = if c == '|' {
            Some("Lambda `|param|` 已废除：使用裸字段或 `别名.字段` 引用".to_string())
        } else if c == '^' {
            Some("Pebble 不存在幂运算符 `^`".to_string())
        } else {
            None
        };
        let mut d = pebble_diagnostic::Diagnostic::error(
            Code::LexIllegalChar,
            format!("非法字符 `{}`", c),
            span!(start, i + 1),
        );
        if let Some(h) = hint {
            d = d.with_hint(h);
        }
        diags.push(d);
        i += 1;
    }
    out.push(Token {
        tok: Tok::Eof,
        span: span!(n, n),
    });
    out
}
