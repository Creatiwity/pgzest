use super::keywords::{keyword_from_str, Keyword};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Keyword(Keyword),        // mot-clé PL/pgSQL
    Ident(String),           // identifiant (non keyword)
    Number(String),          // littéral numérique
    StringLit(String),       // 'chaîne'
    DollarLabel(String),     // $tag$ (label de dollar-quote, vide pour $$)
    Semi,                    // ;
    Comma,                   // ,
    Dot,                     // .
    Assign,                  // :=  ou  =  (dans DECLARE default)
    Colon,                   // :
    ColonEq,                 // :=
    Eq,                      // =
    NotEq,                   // != ou <>
    Lt,                      // <
    Lte,                     // <=
    Gt,                      // >
    Gte,                     // >=
    Plus,                    // +
    Minus,                   // -
    Star,                    // *
    Slash,                   // /
    Percent,                 // %
    LParen,                  // (
    RParen,                  // )
    LBracket,                // [
    RBracket,                // ]
    DotDot,                  // ..  (range dans FOR)
    Percent2,                // %TYPE ou %ROWTYPE
    Eof,
}

pub struct Lexer<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    pub fn pos(&self) -> usize { self.pos }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn peek2(&self) -> Option<char> {
        let mut chars = self.input[self.pos..].chars();
        chars.next();
        chars.next()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.input[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Espaces
            while self.peek().map(|c| c.is_whitespace()).unwrap_or(false) {
                self.advance();
            }
            // Commentaire -- jusqu'à fin de ligne
            if self.input[self.pos..].starts_with("--") {
                while self.peek().map(|c| c != '\n').unwrap_or(false) {
                    self.advance();
                }
                continue;
            }
            // Commentaire /* ... */
            if self.input[self.pos..].starts_with("/*") {
                self.pos += 2;
                while !self.input[self.pos..].starts_with("*/") && self.pos < self.input.len() {
                    self.advance();
                }
                if self.input[self.pos..].starts_with("*/") {
                    self.pos += 2;
                }
                continue;
            }
            break;
        }
    }

    pub fn next_token(&mut self) -> Token {
        self.skip_whitespace_and_comments();

        let ch = match self.peek() {
            None => return Token::Eof,
            Some(c) => c,
        };

        match ch {
            ';' => { self.advance(); Token::Semi }
            ',' => { self.advance(); Token::Comma }
            '(' => { self.advance(); Token::LParen }
            ')' => { self.advance(); Token::RParen }
            '[' => { self.advance(); Token::LBracket }
            ']' => { self.advance(); Token::RBracket }
            '+' => { self.advance(); Token::Plus }
            '*' => { self.advance(); Token::Star }
            '/' => { self.advance(); Token::Slash }
            '%' => {
                self.advance();
                Token::Percent
            }
            '.' => {
                self.advance();
                if self.peek() == Some('.') {
                    self.advance();
                    Token::DotDot
                } else {
                    Token::Dot
                }
            }
            ':' => {
                self.advance();
                if self.peek() == Some('=') {
                    self.advance();
                    Token::ColonEq
                } else {
                    Token::Colon
                }
            }
            '=' => { self.advance(); Token::Eq }
            '!' => {
                self.advance();
                if self.peek() == Some('=') { self.advance(); Token::NotEq }
                else { Token::Ident("!".to_string()) }
            }
            '<' => {
                self.advance();
                if self.peek() == Some('=') { self.advance(); Token::Lte }
                else if self.peek() == Some('>') { self.advance(); Token::NotEq }
                else { Token::Lt }
            }
            '>' => {
                self.advance();
                if self.peek() == Some('=') { self.advance(); Token::Gte }
                else { Token::Gt }
            }
            '-' => {
                self.advance();
                Token::Minus
            }
            '\'' => {
                // Chaîne entre guillemets simples
                self.advance(); // consume '
                let start = self.pos;
                loop {
                    match self.peek() {
                        None => break,
                        Some('\'') => {
                            self.advance();
                            if self.peek() == Some('\'') {
                                self.advance(); // escape '' → '
                            } else {
                                break;
                            }
                        }
                        _ => { self.advance(); }
                    }
                }
                let s = self.input[start..self.pos - 1].to_string();
                Token::StringLit(s)
            }
            '0'..='9' => {
                let start = self.pos;
                while self.peek().map(|c| c.is_ascii_digit() || c == '.').unwrap_or(false) {
                    self.advance();
                }
                Token::Number(self.input[start..self.pos].to_string())
            }
            'a'..='z' | 'A'..='Z' | '_' | '$' => {
                let start = self.pos;
                while self.peek().map(|c| c.is_alphanumeric() || c == '_' || c == '$').unwrap_or(false) {
                    self.advance();
                }
                let word = &self.input[start..self.pos];
                let kw = keyword_from_str(word);
                if kw == Keyword::Other {
                    Token::Ident(word.to_string())
                } else {
                    Token::Keyword(kw)
                }
            }
            '"' => {
                // Identifiant entre guillemets doubles
                self.advance();
                let start = self.pos;
                while self.peek().map(|c| c != '"').unwrap_or(false) {
                    self.advance();
                }
                let s = self.input[start..self.pos].to_string();
                if self.peek() == Some('"') { self.advance(); }
                Token::Ident(s)
            }
            _ => {
                self.advance();
                Token::Ident(ch.to_string())
            }
        }
    }

    /// Tokenize tout le contenu et retourne un Vec<Token> (sans Eof)
    pub fn tokenize_all(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let t = self.next_token();
            if t == Token::Eof { break; }
            tokens.push(t);
        }
        tokens
    }

    /// Tokenize the entire input and return `(tokens, byte_offsets)` where
    /// `byte_offsets[i]` is the byte position in the input string at which
    /// `tokens[i]` starts (after skipping whitespace/comments).
    pub fn tokenize_all_with_offsets(&mut self) -> (Vec<Token>, Vec<usize>) {
        let mut tokens = Vec::new();
        let mut offsets = Vec::new();
        loop {
            self.skip_whitespace_and_comments();
            let start = self.pos;
            let t = self.next_token();
            if t == Token::Eof { break; }
            tokens.push(t);
            offsets.push(start);
        }
        (tokens, offsets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_basic_block() {
        let mut lex = Lexer::new("BEGIN RETURN NEW; END;");
        assert_eq!(lex.next_token(), Token::Keyword(Keyword::Begin));
        assert_eq!(lex.next_token(), Token::Keyword(Keyword::Return));
        assert_eq!(lex.next_token(), Token::Keyword(Keyword::New));
        assert_eq!(lex.next_token(), Token::Semi);
        assert_eq!(lex.next_token(), Token::Keyword(Keyword::End));
        assert_eq!(lex.next_token(), Token::Semi);
        assert_eq!(lex.next_token(), Token::Eof);
    }

    #[test]
    fn lex_assignment() {
        let mut lex = Lexer::new("v_count := 42;");
        assert_eq!(lex.next_token(), Token::Ident("v_count".to_string()));
        assert_eq!(lex.next_token(), Token::ColonEq);
        assert_eq!(lex.next_token(), Token::Number("42".to_string()));
        assert_eq!(lex.next_token(), Token::Semi);
    }

    #[test]
    fn lex_new_field() {
        let mut lex = Lexer::new("NEW.updated_at");
        assert_eq!(lex.next_token(), Token::Keyword(Keyword::New));
        assert_eq!(lex.next_token(), Token::Dot);
        assert_eq!(lex.next_token(), Token::Ident("updated_at".to_string()));
    }
}
