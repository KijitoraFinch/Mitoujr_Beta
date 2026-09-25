use crate::engine::Engine;
use crate::graph::Link;
use crate::model::{NamedTarget, Relation, Selector, Target};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expression {
    And {
        args: Vec<Expression>,
    },
    Or {
        args: Vec<Expression>,
    },
    Not {
        arg: Box<Expression>,
    },
    Exists {
        field: String,
    },
    Compare {
        field: String,
        relation: Predicate,
        value: Operand,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Equal,
    NotEqual,
    Within,
    Contains,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Operand {
    Json(Value),
    Region(Target),
    Selector(Target),
}

pub fn target(input: &str) -> Result<Target> {
    if let Some(name) = input.strip_prefix('@') {
        ensure!(!name.is_empty(), "empty named target");
        Ok(Target::Named(NamedTarget { name: name.into() }))
    } else if input.trim_start().starts_with('{') {
        serde_json::from_str(input).context("invalid target JSON")
    } else {
        Ok(Target::Selector(Selector::parse(input)?))
    }
}

impl Expression {
    pub fn parse(text: &str) -> Result<Self> {
        let mut parser = Parser { text, offset: 0 };
        let expression = parser.or()?;
        parser.space();
        ensure!(
            parser.offset == text.len(),
            "unexpected query input at byte {}",
            parser.offset
        );
        expression.validate()?;
        Ok(expression)
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::And { args } | Self::Or { args } => {
                ensure!(
                    !args.is_empty(),
                    "boolean expression needs at least one argument"
                );
                for arg in args {
                    arg.validate()?;
                }
            }
            Self::Not { arg } => arg.validate()?,
            Self::Exists { field } => {
                Field::parse(field)?;
            }
            Self::Compare {
                field,
                relation,
                value,
            } => {
                let parsed = Field::parse(field)?;
                match (&parsed, value) {
                    (Field::Source | Field::Target, Operand::Region(_)) => {}
                    (Field::Source | Field::Target, Operand::Selector(_)) => ensure!(
                        matches!(relation, Predicate::Equal | Predicate::NotEqual),
                        "stored selectors support == and !="
                    ),
                    (Field::Id | Field::Annotation(_), Operand::Json(_)) => ensure!(
                        matches!(relation, Predicate::Equal | Predicate::NotEqual),
                        "data values support == and !="
                    ),
                    _ => bail!("field {field} cannot be compared with this operand"),
                }
            }
        }
        Ok(())
    }

    /// False dominates AND and true dominates OR, including when another
    /// operand is unknown. An unresolved predicate never silently becomes false.
    pub async fn evaluate(&self, engine: &mut Engine<'_>, link: &Link) -> Result<bool> {
        match self {
            Self::And { args } => combine(args, engine, link, false).await,
            Self::Or { args } => combine(args, engine, link, true).await,
            Self::Not { arg } => Ok(!Box::pin(arg.evaluate(engine, link)).await?),
            Self::Exists { field } => Ok(match Field::parse(field)? {
                Field::Source | Field::Target => true,
                Field::Id => !link.ids().is_empty(),
                Field::Annotation(path) => annotation(link, &path).is_some(),
            }),
            Self::Compare {
                field,
                relation,
                value,
            } => {
                match (Field::parse(field)?, value) {
                    (Field::Id, Operand::Json(expected)) => {
                        let equal = expected.as_str().is_some_and(|id| link.ids().contains(id));
                        equality(*relation, equal)
                    }
                    (Field::Annotation(path), Operand::Json(expected)) => {
                        equality(*relation, annotation(link, &path) == Some(expected))
                    }
                    (field @ (Field::Source | Field::Target), Operand::Region(target)) => {
                        let selector = target.expand(&engine.session.workspace.config.names)?;
                        let expected = engine.resolve(&selector).await?;
                        let actual = match field {
                            Field::Source => &link.source,
                            _ => &link.target,
                        }
                        .region()?;
                        let comparison = engine.compare(actual, &expected).await?;
                        Ok(match relation {
                            Predicate::Equal => comparison == Relation::Equal,
                            Predicate::NotEqual => comparison != Relation::Equal,
                            Predicate::Within => {
                                matches!(comparison, Relation::Equal | Relation::Within)
                            }
                            Predicate::Contains => {
                                matches!(comparison, Relation::Equal | Relation::Contains)
                            }
                        })
                    }
                    (field @ (Field::Source | Field::Target), Operand::Selector(expected)) => {
                        // A group can contain equivalent declarations with different spellings.
                        let equal = link.occurrences.iter().any(|occurrence| match &field {
                            Field::Source => {
                                expected == &Target::Selector(occurrence.declaration.source.clone())
                            }
                            Field::Target => expected == &occurrence.declaration.reference.target,
                            _ => unreachable!(),
                        });
                        equality(*relation, equal)
                    }
                    _ => bail!("invalid comparison; validate the expression before evaluation"),
                }
            }
        }
    }
}

fn equality(predicate: Predicate, equal: bool) -> Result<bool> {
    match predicate {
        Predicate::Equal => Ok(equal),
        Predicate::NotEqual => Ok(!equal),
        _ => bail!("non-equality predicate on a data value"),
    }
}

async fn combine(
    args: &[Expression],
    engine: &mut Engine<'_>,
    link: &Link,
    decisive: bool,
) -> Result<bool> {
    let mut errors = Vec::new();
    for arg in args {
        match Box::pin(arg.evaluate(engine, link)).await {
            Ok(value) if value == decisive => return Ok(decisive),
            Ok(_) => {}
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(!decisive)
}

enum Field {
    Id,
    Source,
    Target,
    Annotation(Vec<String>),
}

impl Field {
    fn parse(input: &str) -> Result<Self> {
        match input {
            "id" => return Ok(Self::Id),
            "source" => return Ok(Self::Source),
            "target" | "reference.target" => return Ok(Self::Target),
            _ => {}
        }
        let mut rest = input
            .strip_prefix("annotation")
            .context("unknown query field")?;
        let mut path = Vec::new();
        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix('.') {
                let end = tail.find(['.', '[']).unwrap_or(tail.len());
                ensure!(end > 0, "empty annotation path component");
                path.push(tail[..end].to_owned());
                rest = &tail[end..];
            } else if let Some(tail) = rest.strip_prefix('[') {
                let mut values = serde_json::Deserializer::from_str(tail).into_iter::<Value>();
                let value = values.next().context("empty path index")??;
                let end = values.byte_offset();
                path.push(match value {
                    Value::String(key) => key,
                    Value::Number(index) if index.is_u64() => index.to_string(),
                    _ => bail!("annotation path index must be a string or nonnegative integer"),
                });
                rest = tail[end..]
                    .trim_start()
                    .strip_prefix(']')
                    .context("missing ] in annotation path")?;
            } else {
                bail!("invalid annotation path: {input}");
            }
        }
        Ok(Self::Annotation(path))
    }
}

fn annotation<'a>(link: &'a Link, path: &[String]) -> Option<&'a Value> {
    let mut value = &link.declaration().annotation;
    for key in path {
        value = match value {
            Value::Object(object) => object.get(key)?,
            Value::Array(array) => array.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(value)
}

struct Parser<'a> {
    text: &'a str,
    offset: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while let Some(character) = self.text[self.offset..].chars().next() {
            if !character.is_whitespace() {
                break;
            }
            self.offset += character.len_utf8();
        }
    }

    fn take(&mut self, token: &str) -> bool {
        self.space();
        if !self.text[self.offset..].starts_with(token) {
            return false;
        }
        if token.chars().last().is_some_and(char::is_alphabetic)
            && self.text[self.offset + token.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            return false;
        }
        self.offset += token.len();
        true
    }

    fn require(&mut self, token: &str) -> Result<()> {
        ensure!(self.take(token), "expected {token} at byte {}", self.offset);
        Ok(())
    }

    fn or(&mut self) -> Result<Expression> {
        let mut args = vec![self.and()?];
        while self.take("or") {
            args.push(self.and()?);
        }
        Ok(if args.len() == 1 {
            args.remove(0)
        } else {
            Expression::Or { args }
        })
    }

    fn and(&mut self) -> Result<Expression> {
        let mut args = vec![self.unary()?];
        while self.take("and") {
            args.push(self.unary()?);
        }
        Ok(if args.len() == 1 {
            args.remove(0)
        } else {
            Expression::And { args }
        })
    }

    fn unary(&mut self) -> Result<Expression> {
        if self.take("not") {
            return Ok(Expression::Not {
                arg: Box::new(self.unary()?),
            });
        }
        if self.take("(") {
            let expression = self.or()?;
            self.require(")")?;
            return Ok(expression);
        }
        if self.take("exists") {
            self.require("(")?;
            let field = self.field()?;
            self.require(")")?;
            return Ok(Expression::Exists { field });
        }
        let field = self.field()?;
        let relation = if self.take("==") {
            Predicate::Equal
        } else if self.take("!=") {
            Predicate::NotEqual
        } else if self.take("within") {
            Predicate::Within
        } else if self.take("contains") {
            Predicate::Contains
        } else {
            bail!(
                "expected ==, !=, within, or contains at byte {}",
                self.offset
            )
        };
        let value = if self.take("region") {
            Operand::Region(self.target()?)
        } else if self.take("selector") {
            Operand::Selector(self.target()?)
        } else {
            Operand::Json(self.json()?)
        };
        Ok(Expression::Compare {
            field,
            relation,
            value,
        })
    }

    fn field(&mut self) -> Result<String> {
        self.space();
        let start = self.offset;
        while let Some(character) = self.text[self.offset..].chars().next() {
            if character == '[' {
                self.offset += 1;
                self.json()?;
                self.require("]")?;
            } else if character.is_alphanumeric() || matches!(character, '_' | '-' | '.') {
                self.offset += character.len_utf8();
            } else {
                break;
            }
        }
        ensure!(self.offset > start, "expected field at byte {start}");
        Ok(self.text[start..self.offset].to_owned())
    }

    fn json(&mut self) -> Result<Value> {
        self.space();
        let mut values =
            serde_json::Deserializer::from_str(&self.text[self.offset..]).into_iter::<Value>();
        let value = values
            .next()
            .context("expected JSON value")?
            .with_context(|| format!("invalid JSON value at byte {}", self.offset))?;
        self.offset += values.byte_offset();
        Ok(value)
    }

    fn target(&mut self) -> Result<Target> {
        self.require("(")?;
        let value = self.json()?;
        self.require(")")?;
        match value {
            Value::String(input) => target(&input),
            Value::Object(_) => serde_json::from_value(value).context("invalid target"),
            _ => bail!("region/selector expects a selector string, object, or @name"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_boolean_precedence_nested_json_and_arbitrary_annotation_keys() -> Result<()> {
        let expression = Expression::parse(
            r#"not annotation["odd.key"][0] == {"v": [true, null]} and exists(annotation.x) or target within region("@module")"#,
        )?;
        let Expression::Or { args } = expression else {
            panic!("OR should be outermost")
        };
        assert!(matches!(&args[0], Expression::And { .. }));
        assert!(matches!(
            &args[1],
            Expression::Compare {
                relation: Predicate::Within,
                ..
            }
        ));
        for invalid in [
            "",
            "annotation..x == 1",
            "source == 5",
            "annotation.x within 3",
            "annotation.x == 1 garbage",
            "target == region(3)",
            "annotation[false] == 1",
            "notable == true",
        ] {
            assert!(Expression::parse(invalid).is_err(), "accepted {invalid}");
        }
        Ok(())
    }
}
