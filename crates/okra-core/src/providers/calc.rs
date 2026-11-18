use super::{Action, Item, Kind, Provider};

/// Inline calculator: typing `12*(3+4)` shows `= 84`, Enter copies the result.
pub struct CalcProvider;

impl Provider for CalcProvider {
    fn name(&self) -> &'static str {
        "Calculator"
    }

    fn query(&self, q: &str, _limit: usize) -> Vec<Item> {
        let text = q.trim();
        if !looks_like_math(text) {
            return Vec::new();
        }
        match evaluate(text) {
            Some(value) if value.is_finite() => {
                let shown = format_number(value);
                vec![Item {
                    id: format!("calc:{text}"),
                    title: format!("= {shown}"),
                    subtitle: format!("{text}  \u{b7}  Enter to copy"),
                    kind: Kind::Calculation,
                    action: Action::Copy(shown),
                    score: 10_000,
                }]
            }
            _ => Vec::new(),
        }
    }
}

/// Cheap guard so ordinary words and file names never reach the parser.
fn looks_like_math(text: &str) -> bool {
    if !text.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || " +-*/^%().\u{d7}\u{f7}".contains(c))
    {
        return false;
    }
    // Needs a real operator: a lone leading minus ("-5") is not a calculation.
    text.char_indices().any(|(i, c)| match c {
        '+' | '*' | '/' | '^' | '%' | '\u{d7}' | '\u{f7}' => true,
        '-' => i > 0,
        _ => false,
    })
}

fn evaluate(text: &str) -> Option<f64> {
    let mut parser = Parser {
        chars: text.chars().collect(),
        pos: 0,
    };
    let value = parser.expr()?;
    parser.skip_spaces();
    if parser.pos == parser.chars.len() {
        Some(value)
    } else {
        None
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let text = format!("{value:.10}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn skip_spaces(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos] == ' ' {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_spaces();
        self.chars.get(self.pos).copied()
    }

    // expr := term (('+' | '-') term)*
    fn expr(&mut self) -> Option<f64> {
        let mut value = self.term()?;
        while let Some(op) = self.peek() {
            match op {
                '+' => {
                    self.pos += 1;
                    value += self.term()?;
                }
                '-' => {
                    self.pos += 1;
                    value -= self.term()?;
                }
                _ => break,
            }
        }
        Some(value)
    }

    // term := power (('*' | '/' | '%') power)*
    fn term(&mut self) -> Option<f64> {
        let mut value = self.power()?;
        while let Some(op) = self.peek() {
            match op {
                '*' | '\u{d7}' => {
                    self.pos += 1;
                    value *= self.power()?;
                }
                '/' | '\u{f7}' => {
                    self.pos += 1;
                    value /= self.power()?;
                }
                '%' => {
                    self.pos += 1;
                    value %= self.power()?;
                }
                _ => break,
            }
        }
        Some(value)
    }

    // power := unary ('^' power)?   (right associative)
    fn power(&mut self) -> Option<f64> {
        let base = self.unary()?;
        if self.peek() == Some('^') {
            self.pos += 1;
            let exponent = self.power()?;
            return Some(base.powf(exponent));
        }
        Some(base)
    }

    fn unary(&mut self) -> Option<f64> {
        match self.peek()? {
            '-' => {
                self.pos += 1;
                Some(-self.unary()?)
            }
            '+' => {
                self.pos += 1;
                self.unary()
            }
            _ => self.atom(),
        }
    }

    fn atom(&mut self) -> Option<f64> {
        if self.peek()? == '(' {
            self.pos += 1;
            let value = self.expr()?;
            if self.peek()? != ')' {
                return None;
            }
            self.pos += 1;
            return Some(value);
        }
        let start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_digit() || self.chars[self.pos] == '.')
        {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(q: &str) -> Option<String> {
        CalcProvider.query(q, 5).into_iter().next().map(|i| i.title)
    }

    #[test]
    fn evaluates_with_precedence_and_parentheses() {
        assert_eq!(calc("2+3*4").as_deref(), Some("= 14"));
        assert_eq!(calc("(2+3)*4").as_deref(), Some("= 20"));
        assert_eq!(calc("2^3^2").as_deref(), Some("= 512"));
        assert_eq!(calc("10/4").as_deref(), Some("= 2.5"));
        assert_eq!(calc("7 % 4").as_deref(), Some("= 3"));
        assert_eq!(calc("-3+5").as_deref(), Some("= 2"));
    }

    #[test]
    fn ignores_words_and_bad_input() {
        assert!(calc("chrome").is_none());
        assert!(calc("7zip").is_none());
        assert!(calc("-5").is_none());
        assert!(calc("1/0").is_none());
        assert!(calc("2+").is_none());
        assert!(calc("(2+3").is_none());
    }
}
