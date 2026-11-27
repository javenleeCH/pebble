//! 词法分析：UTF-8、`//` 行注释、双引号字符串、i64/f64 字面量。
//! `|`（Lambda 已废除）与 `^`（不存在幂运算）是非法字符，给出针对性提示。
//!
//! 错误恢复（第 8 章；08-diagnostics §8.6 第 1 条）：
//! - 词法错误**就地恢复**，一次扫描报出全部 LEX 错误；只有无法确定 token 边界的
//!   未闭合字符串才退到下一行行首继续。
//! - 能救回值的就救回值（坏转义的字符串、`1.` 的整数部分、`.5` 的小数部分），
//!   让 token 流的结构保持完整；救不回的留一个 `Tok::Error`「洞」占位。
//! - Span 一律记录 UTF-8 **字节偏移**。

use pebble_diagnostic::{Code, DiagBag, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    /// 整数字面量；超出 i64 范围时词法层保留 i128 值，语义层报错。
    Int(i128),
    Float(f64),
    Str(String),
    /// 词法层留下的「洞」：这里有一个无法识别的 token，词法层**已经报过错**。
    /// 它占住位置，使 token 流的结构不变；解析器遇到它绝不重复报错（第 8 章）。
    Error,
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
    // 字符下标 → 字节偏移。扫描按字符走（简单直观），但 Span 必须写成字节偏移：
    // 渲染层用 `&source[start..end]` 切片，而 Rust 字符串是按字节索引的。
    let mut byte_at: Vec<usize> = source.char_indices().map(|(b, _)| b).collect();
    byte_at.push(source.len());
    let n = bytes.len();
    let mut i = 0usize;

    macro_rules! span {
        ($s:expr, $e:expr) => {
            Span::new(byte_at[$s], byte_at[$e])
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
        // UTF-8 BOM：Windows 上的编辑器常在文件开头加一个。它不是程序的一部分，跳过即可。
        // （不能从 `source` 里削掉——Span 必须相对原始源码计数）
        if c == '\u{feff}' && i == 0 {
            i += 1;
            continue;
        }
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
            scan_digits(&bytes, &mut i);
            let digits_end = i;
            let text: String = bytes[start..digits_end].iter().collect();
            let value = text.parse::<i128>();
            if i < n && bytes[i] == '.' {
                if i + 1 < n && bytes[i + 1].is_ascii_digit() {
                    // 浮点：整数部分 `.` 小数部分
                    i += 1;
                    scan_digits(&bytes, &mut i);
                    let text: String = bytes[start..i].iter().collect();
                    let value: f64 = text.parse().unwrap_or(f64::INFINITY);
                    out.push(Token {
                        tok: Tok::Float(value),
                        span: span!(start, i),
                    });
                } else if let Ok(v) = value {
                    // `1.` 形式：LEX-004。救回整数部分（`1.` 当作 `1`），token 流不留洞
                    diags.error(
                        Code::LexBadNumber,
                        format!("数字字面量 `{}.` 后的小数点缺少数字", v),
                        span!(start, i + 1),
                    );
                    i += 1;
                    out.push(Token {
                        tok: Tok::Int(v),
                        span: span!(start, digits_end),
                    });
                } else {
                    // 整数部分本身已溢出：按「过大」报告
                    diags.error(
                        Code::LexBadNumber,
                        format!("数字字面量 `{}` 过大，无法解析", text),
                        span!(start, digits_end),
                    );
                    out.push(Token {
                        tok: Tok::Error,
                        span: span!(start, digits_end),
                    });
                }
            } else {
                match value {
                    Ok(v) => out.push(Token {
                        tok: Tok::Int(v),
                        span: span!(start, digits_end),
                    }),
                    Err(_) => {
                        // 连 i128 都放不下：没有值可以救，留一个洞
                        diags.error(
                            Code::LexBadNumber,
                            format!("数字字面量 `{}` 过大，无法解析", text),
                            span!(start, digits_end),
                        );
                        out.push(Token {
                            tok: Tok::Error,
                            span: span!(start, digits_end),
                        });
                    }
                }
            }
            continue;
        }
        // `.` 开头且后跟数字 → `.5` 形式非法（LEX-004）。
        // 只越过这个 `.`：数字部分在下一轮照常识别成整数，token 流不留洞。
        if c == '.' && i + 1 < n && bytes[i + 1].is_ascii_digit() {
            i += 1;
            let digits_end = i + digits_len(&bytes, i);
            let text: String = bytes[start..digits_end].iter().collect();
            diags.error(
                Code::LexBadNumber,
                format!("数字字面量 `{text}` 缺少整数部分"),
                span!(start, digits_end),
            );
            if text[1..].parse::<i128>().is_err() {
                // 小数部分本身已经放不进 i128：没有值可以救。一个根因只报一次，整段留一个洞
                i = digits_end;
                out.push(Token {
                    tok: Tok::Error,
                    span: span!(start, digits_end),
                });
            }
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
                    if i + 1 < n && bytes[i + 1] != '\n' {
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
                                // LEX-003：丢掉这个转义，字符串本身保留
                                diags.error(
                                    Code::LexBadEscape,
                                    format!("非法转义序列 `\\{}`", esc),
                                    span!(i, i + 2),
                                );
                                i += 2;
                            }
                        }
                    } else {
                        // 反斜杠在文件结尾或行尾：字符串没闭合，按 LEX-002 报告。
                        // 把反斜杠一并算进这个字符串，免得它再被当成一个独立的非法字符。
                        // 也不把换行当转义字符——那会往消息里塞进一个真正的换行
                        i += 1;
                        break;
                    }
                } else {
                    value.push(ch);
                    i += 1;
                }
            }
            if !closed {
                // LEX-002：字符串到行尾都没闭合。留一个洞占住它的位置，
                // 扫描从下一行行首继续（`i` 正停在换行符上，下一轮按空白跳过）。
                diags.error(
                    Code::LexUnclosedString,
                    "字符串未闭合（行尾未遇到结束引号）",
                    span!(start, i),
                );
                out.push(Token {
                    tok: Tok::Error,
                    span: span!(start, i),
                });
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
        // 非法字符（含已废除的 `|`、不存在的 `^`）：LEX-001，留一个洞
        let hint = if c == '|' {
            Some("Lambda `|param|` 已废除：使用裸字段或 `别名.字段` 引用".to_string())
        } else if c == '^' {
            Some("Pebble 不存在幂运算符 `^`".to_string())
        } else if c == '&' {
            Some("逻辑与写作 `&&`（两个 `&`）".to_string())
        } else if c == ';' {
            Some("Pebble 的流水线靠 `.` 连接，不需要分号".to_string())
        } else if let Some(h) = full_width_hint(c) {
            Some(h.to_string())
        } else if c.is_whitespace() {
            Some(
                "Pebble 只把普通空格、制表符和换行当作空白：请检查是否输入了全角或不换行空格"
                    .to_string(),
            )
        } else {
            None
        };
        // 控制字符与各种「隐形空白」在终端里看不见：报它的码点，而不是往消息里塞一个隐形的东西
        let shown = if c.is_control() || c.is_whitespace() {
            format!("U+{:04X}", c as u32)
        } else {
            format!("`{c}`")
        };
        let mut d = pebble_diagnostic::Diagnostic::error(
            Code::LexIllegalChar,
            format!("非法字符 {shown}"),
            span!(start, i + 1),
        );
        if let Some(h) = hint {
            d = d.with_hint(h);
        }
        diags.push(d);
        out.push(Token {
            tok: Tok::Error,
            span: span!(start, i + 1),
        });
        i += 1;
    }
    out.push(Token {
        tok: Tok::Eof,
        span: span!(n, n),
    });
    out
}

/// 常见的全角标点 → 改用半角的提示。
///
/// 中文输入法下这些笔误相当常见，而全角与半角在等宽字体里只差一点点，肉眼几乎发现不了。
fn full_width_hint(c: char) -> Option<&'static str> {
    Some(match c {
        '（' | '）' => "这是全角括号：请改用半角 `(` 和 `)`",
        '，' => "这是全角逗号：请改用半角 `,`",
        '：' => "这是全角冒号：Pebble 的参数绑定写作 `名字 = 表达式`，不用冒号",
        '“' | '”' | '‘' | '’' => "这是中文引号：字符串字面量请用半角双引号 `\"`",
        '！' => "这是全角感叹号：逻辑非写作半角 `!`",
        '＋' | '－' | '＊' | '／' | '％' | '＝' | '＞' | '＜' => {
            "这是全角运算符：请改用半角符号"
        }
        '．' => "这是全角句点：算子之间用半角 `.` 连接",
        _ => return None,
    })
}

/// 从 `i` 起吞掉连续数字，返回新的下标。
fn scan_digits(bytes: &[char], i: &mut usize) {
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
        *i += 1;
    }
}

/// 从 `i` 起的连续数字个数。
fn digits_len(bytes: &[char], i: usize) -> usize {
    let mut end = i;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    end - i
}
