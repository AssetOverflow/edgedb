use std::collections::BTreeSet;

use edgeql_parser::tokenizer::{Kind, Tokenizer, Token, Value};
use edgeql_parser::position::{Pos, Span};

use blake2::{Blake2b512, Digest};

#[derive(Debug, PartialEq)]
pub struct Variable {
    pub value: Value,
}

pub struct Entry<'a> {
    pub processed_source: String,
    pub hash: [u8; 64],
    pub tokens: Vec<Token<'a>>,
    pub variables: Vec<Vec<Variable>>,
    pub end_pos: Pos,
    pub named_args: bool,
    pub first_arg: Option<usize>,
}

#[derive(Debug)]
pub enum Error {
    Tokenizer(String, Pos),
    Assertion(String, Pos),
}

fn push_var<'x>(res: &mut Vec<Token<'x>>, module: &'x str, typ: &'x str,
    var: String, span: Span)
{
    res.push(Token {kind: Kind::OpenParen, text: "(".into(), span, value: None});
    res.push(Token {kind: Kind::Less, text: "<".into(), span, value: None});
    res.push(Token {kind: Kind::Ident, text: module.into(), span, value: None});
    res.push(Token {kind: Kind::Namespace, text: "::".into(), span, value: None});
    res.push(Token {kind: Kind::Ident, text: typ.into(), span,
        value: Some(Value::String(typ.to_string())),
    });
    res.push(Token {kind: Kind::Greater, text: ">".into(), span, value: None});
    res.push(Token {kind: Kind::Argument, text: var.into(), span, value: None});
    res.push(Token {kind: Kind::CloseParen, text: ")".into(), span, value: None});
}

fn scan_vars<'x, 'y: 'x, I>(tokens: I) -> Option<(bool, usize)>
    where I: IntoIterator<Item=&'x Token<'y>>,
{
    let mut max_visited = None::<usize>;
    let mut names = BTreeSet::new();
    for t in tokens {
        if t.kind == Kind::Argument {
            if let Ok(v) = t.text[1..].parse() {
                if max_visited.map(|old| v > old).unwrap_or(true) {
                    max_visited = Some(v);
                }
            } else {
                names.insert(&t.text[..]);
            }
        }
    }
    if names.is_empty() {
        let next = max_visited.map(|x| x.checked_add(1)).unwrap_or(Some(0))?;
        Some((false, next))
    } else if max_visited.is_some() {
        return None  // mixed arguments
    } else {
        Some((true, names.len()))
    }
}

fn hash(text: &str) -> [u8; 64] {
    let mut result = [0u8; 64];
    result.copy_from_slice(&Blake2b512::new_with_prefix(text.as_bytes())
                           .finalize());
    return result;
}

pub fn normalize(text: &str) -> Result<Entry, Error> {
    let mut token_stream = Tokenizer::new(text).validated_values();
    let tokens = (&mut token_stream)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Tokenizer(e.message, e.span.start))?;
    let end_pos = token_stream.current_pos();
    let (named_args, var_idx) = match scan_vars(&tokens) {
        Some(pair) => pair,
        None => {
            // don't extract from invalid query, let python code do its work
            let processed_source = serialize_tokens(&tokens);
            return Ok(Entry {
                hash: hash(&processed_source),
                processed_source,
                tokens,
                variables: Vec::new(),
                end_pos,
                named_args: false,
                first_arg: None,
            });
        }
    };
    let mut rewritten_tokens = Vec::with_capacity(tokens.len());
    let mut all_variables = Vec::new();
    let mut variables = Vec::new();
    let mut counter = var_idx;
    let mut next_var = || {
        let n = counter;
        counter += 1;
        if named_args {
            format!("$__edb_arg_{}", n)
        } else {
            format!("${}", n)
        }
    };
    let mut last_was_set = false;
    for (idx, tok) in tokens.iter().enumerate() {
        let mut is_set = false;
        match tok.kind {
            Kind::IntConst
            // Don't replace `.12` because this is a tuple access
            if !matches!(rewritten_tokens.last(),
                Some(Token { kind: Kind::Dot, .. }))
            // Don't replace 'LIMIT 1' as a special case
            && (tok.text != "1"
                || !matches!(rewritten_tokens.last(),
                    Some(Token { kind: Kind::Keyword, ref text, .. })
                    if text.eq_ignore_ascii_case("LIMIT")))
            && tok.text != "9223372036854775808"
            => {
                push_var(&mut rewritten_tokens, "__std__", "int64",
                    next_var(),
                    tok.span);
                variables.push(Variable {
                    value: tok.value.clone().unwrap(),
                });
                continue;
            }
            Kind::FloatConst => {
                push_var(&mut rewritten_tokens, "__std__", "float64",
                    next_var(),
                    tok.span);
                variables.push(Variable {
                    value: tok.value.clone().unwrap(),
                });
                continue;
            }
            Kind::BigIntConst => {
                push_var(&mut rewritten_tokens, "__std__", "bigint",
                    next_var(),
                    tok.span);
                variables.push(Variable {
                    value: tok.value.clone().unwrap(),
                });
                continue;
            }
            Kind::DecimalConst => {
                push_var(&mut rewritten_tokens, "__std__", "decimal",
                    next_var(),
                    tok.span);
                variables.push(Variable {
                    value: tok.value.clone().unwrap(),
                });
                continue;
            }
            Kind::Str => {
                push_var(&mut rewritten_tokens, "__std__", "str",
                    next_var(),
                    tok.span);
                variables.push(Variable {
                    value: tok.value.clone().unwrap(),
                });
                continue;
            }
            Kind::Keyword
            if (matches!(&(&tok.text[..].to_uppercase())[..],
                         "CONFIGURE"|"CREATE"|"ALTER"|"DROP"|"START"|"ANALYZE")
                || (last_was_set &&
                    matches!(&(&tok.text[..].to_uppercase())[..],
                             "GLOBAL"))
            )
            => {
                let processed_source = serialize_tokens(&tokens);
                return Ok(Entry {
                    hash: hash(&processed_source),
                    processed_source,
                    tokens,
                    variables: Vec::new(),
                    end_pos,
                    named_args: false,
                    first_arg: None,
                });
            }
            // Split on semicolons.
            // N.B: This naive statement splitting on semicolons works
            // because the only statements with internal semis are DDL
            // statements, which we don't support anyway.
            Kind::Semicolon => {
                if idx + 1 < tokens.len() {
                    all_variables.push(variables);
                    variables = Vec::new();
                }
                rewritten_tokens.push(tok.clone());
            }
            Kind::Keyword
            if (matches!(&(&tok.text[..].to_uppercase())[..], "SET")) => {
                is_set = true;
                rewritten_tokens.push(tok.clone());
            }
            _ => rewritten_tokens.push(tok.clone()),
        }
        last_was_set = is_set;
    }

    all_variables.push(variables);
    let processed_source = serialize_tokens(&rewritten_tokens[..]);
    return Ok(Entry {
        hash: hash(&processed_source),
        processed_source,
        named_args,
        first_arg: if counter <= var_idx { None } else { Some(var_idx) },
        tokens: rewritten_tokens,
        variables: all_variables,
        end_pos,
    });
}

fn is_operator(token: &Token) -> bool {
    use edgeql_parser::tokenizer::Kind::*;
    match token.kind {
        | Assign
        | SubAssign
        | AddAssign
        | Arrow
        | Coalesce
        | Namespace
        | DoubleSplat
        | BackwardLink
        | FloorDiv
        | Concat
        | GreaterEq
        | LessEq
        | NotEq
        | NotDistinctFrom
        | DistinctFrom
        | Comma
        | OpenParen
        | CloseParen
        | OpenBracket
        | CloseBracket
        | OpenBrace
        | CloseBrace
        | Dot
        | Semicolon
        | Colon
        | Add
        | Sub
        | Mul
        | Div
        | Modulo
        | Pow
        | Less
        | Greater
        | Eq
        | Ampersand
        | Pipe
        | At
        => true,
        | DecimalConst
        | FloatConst
        | IntConst
        | BigIntConst
        | BinStr
        | Argument
        | Str
        | BacktickName
        | Keyword
        | Ident
        | Substitution
        => false,
    }
}

fn serialize_tokens(tokens: &[Token<'_>]) -> String {
    use edgeql_parser::tokenizer::Kind::Argument;

    let mut buf = String::new();
    let mut needs_space = false;
    for token in tokens {
        if needs_space && !is_operator(token) && token.kind != Argument {
            buf.push(' ');
        }
        buf.push_str(&token.text);
        needs_space = !is_operator(token);
    }
    return buf;
}

#[cfg(test)]
mod test {
    use super::scan_vars;
    use edgeql_parser::tokenizer::{Token, Tokenizer};

    fn tokenize<'x>(s: &'x str) -> Vec<Token<'x>> {
        let mut r = Vec::new();
        let mut s = Tokenizer::new(s);
        loop {
            match s.next() {
                Some(Ok(x)) => r.push(x.into()),
                None => break,
                Some(Err(e)) => panic!("Parse error at {}: {}", s.current_pos(), e.message),
            }
        }
        return r;
    }

    #[test]
    fn none() {
        assert_eq!(scan_vars(&tokenize("SELECT 1+1")).unwrap(), (false, 0));
    }

    #[test]
    fn numeric() {
        assert_eq!(scan_vars(&tokenize("$0 $1 $2")).unwrap(), (false, 3));
        assert_eq!(scan_vars(&tokenize("$2 $3 $2")).unwrap(), (false, 4));
        assert_eq!(scan_vars(&tokenize("$0 $0 $0")).unwrap(), (false, 1));
        assert_eq!(scan_vars(&tokenize("$10 $100")).unwrap(), (false, 101));
    }

    #[test]
    fn named() {
        assert_eq!(scan_vars(&tokenize("$a")).unwrap(), (true, 1));
        assert_eq!(scan_vars(&tokenize("$b $c $d")).unwrap(), (true, 3));
        assert_eq!(scan_vars(&tokenize("$b $c $b")).unwrap(), (true, 2));
        assert_eq!(scan_vars(&tokenize("$a $b $b $a $c $xx")).unwrap(),
            (true, 4));
    }

    #[test]
    fn mixed() {
        assert_eq!(scan_vars(&tokenize("$a $0")), None);
        assert_eq!(scan_vars(&tokenize("$0 $a")), None);
        assert_eq!(scan_vars(&tokenize("$b $c $100")), None);
        assert_eq!(scan_vars(&tokenize("$10 $xx $yy")), None);
    }

}
