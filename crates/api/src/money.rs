//! Invoice money math. All amounts are integer cents (`i64`); every operation is checked.

use serde::Deserialize;

pub const MAX_LINE_ITEMS: usize = 100;
pub const MAX_DESCRIPTION_CHARS: usize = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PricedLine {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub line_total_cents: i64,
}

/// Validates line items and returns them priced, plus the invoice total.
pub fn price_lines(items: &[LineInput]) -> Result<(Vec<PricedLine>, i64), String> {
    if items.is_empty() || items.len() > MAX_LINE_ITEMS {
        return Err(format!(
            "line_items must contain 1 to {MAX_LINE_ITEMS} items"
        ));
    }

    let mut total: i64 = 0;
    let mut priced = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let description = item.description.trim();
        if description.is_empty()
            || description.chars().count() > MAX_DESCRIPTION_CHARS
            || description.contains(char::is_control)
        {
            return Err(format!(
                "line_items[{i}].description must be 1 to {MAX_DESCRIPTION_CHARS} characters without control characters"
            ));
        }
        if item.quantity <= 0 {
            return Err(format!("line_items[{i}].quantity must be greater than 0"));
        }
        if item.unit_amount_cents < 0 {
            return Err(format!(
                "line_items[{i}].unit_amount_cents must not be negative"
            ));
        }
        let line_total = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .ok_or_else(|| format!("line_items[{i}] total is too large"))?;
        total = total
            .checked_add(line_total)
            .ok_or_else(|| "invoice total is too large".to_string())?;
        priced.push(PricedLine {
            description: description.to_string(),
            quantity: item.quantity,
            unit_amount_cents: item.unit_amount_cents,
            line_total_cents: line_total,
        });
    }
    Ok((priced, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(quantity: i64, unit_amount_cents: i64) -> LineInput {
        LineInput {
            description: "item".into(),
            quantity,
            unit_amount_cents,
        }
    }

    #[test]
    fn computes_line_and_invoice_totals() {
        let (lines, total) = price_lines(&[line(2, 1500), line(1, 99), line(3, 0)]).unwrap();
        assert_eq!(
            lines.iter().map(|l| l.line_total_cents).collect::<Vec<_>>(),
            [3000, 99, 0]
        );
        assert_eq!(total, 3099);
    }

    #[test]
    fn rejects_non_positive_quantity_and_negative_amount() {
        assert!(price_lines(&[line(0, 100)]).is_err());
        assert!(price_lines(&[line(-1, 100)]).is_err());
        assert!(price_lines(&[line(1, -1)]).is_err());
    }

    #[test]
    fn rejects_overflow_instead_of_wrapping() {
        assert!(price_lines(&[line(2, i64::MAX)]).is_err());
        assert!(price_lines(&[line(1, i64::MAX), line(1, 1)]).is_err());
    }

    #[test]
    fn rejects_empty_too_many_and_blank_description() {
        assert!(price_lines(&[]).is_err());
        let many: Vec<_> = (0..=MAX_LINE_ITEMS).map(|_| line(1, 1)).collect();
        assert!(price_lines(&many).is_err());
        let blank = LineInput {
            description: "   ".into(),
            quantity: 1,
            unit_amount_cents: 1,
        };
        assert!(price_lines(&[blank]).is_err());
    }

    #[test]
    fn rejects_control_characters_in_description() {
        let nul = LineInput {
            description: "bad\0item".into(),
            quantity: 1,
            unit_amount_cents: 1,
        };
        assert!(price_lines(&[nul]).is_err());
    }
}
