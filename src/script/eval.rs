//! Expression evaluation with GNU ld semantics.
//!
//! Layout owns the state expressions read (symbols, section addresses, the
//! location counter), so evaluation goes through an [`EvalContext`] the
//! caller implements. The evaluator supplies the language rules:
//!
//! - All arithmetic is on 64-bit values and wraps. `/` and `%` are signed;
//!   division by zero is an error. Shift counts are taken modulo 64, as on
//!   the x86-64 hosts GNU ld usually runs on.
//! - `&&`, `||` and `?:` evaluate only the operands they need.
//! - Values carry a *section*, following the "Expression Section" rules of
//!   the GNU ld manual: a value is a plain number, an absolute address, or an
//!   offset relative to an output section. For example, inside an output
//!   section `.` is relative to that section, and `sym = 0x10;` there makes
//!   `sym` 0x10 bytes past the section start. See [`Value`].
//!
//! Stateful built-ins (`DATA_SEGMENT_ALIGN` and friends, `SEGMENT_START`)
//! delegate to the context, whose default methods implement the plain
//! formulas from the manual.

#![deny(clippy::arithmetic_side_effects)]

use super::ast::{Assignment, BinaryOp, Expr, Fill, UnaryOp};
use super::error::EvalError;

/// Which section a value belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueSection<S> {
    /// A plain number, such as a literal or `SIZEOF(.text)`.
    Number,
    /// An absolute address.
    Absolute,
    /// An offset from the start of an output section.
    Relative(S),
}

/// The result of evaluating an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value<S> {
    /// For [`ValueSection::Relative`], the offset from the section's
    /// address; otherwise the value itself.
    pub value: u64,
    /// The section the value belongs to.
    pub section: ValueSection<S>,
    /// GNU ld's `rel_from_abs`: the value came from the location counter
    /// (`.`, `ALIGN`, `ORIGIN`, `SEGMENT_START`) while outside an output
    /// section, so it is absolute only because there was no section to be
    /// relative to. GNU ld turns such a symbol into one relative to the
    /// section that contains `.` once layout is final; layout should do the
    /// same. Meaningless unless `section` is [`ValueSection::Absolute`].
    pub from_dot: bool,
}

impl<S: Copy> Value<S> {
    /// A plain number.
    #[must_use]
    pub fn number(value: u64) -> Self {
        Self {
            value,
            section: ValueSection::Number,
            from_dot: false,
        }
    }

    /// An absolute address.
    #[must_use]
    pub fn absolute(value: u64) -> Self {
        Self {
            value,
            section: ValueSection::Absolute,
            from_dot: false,
        }
    }

    /// An offset into `section`.
    #[must_use]
    pub fn relative(section: S, offset: u64) -> Self {
        Self {
            value: offset,
            section: ValueSection::Relative(section),
            from_dot: false,
        }
    }

    /// Sets [`Value::from_dot`].
    #[must_use]
    pub fn with_from_dot(mut self, from_dot: bool) -> Self {
        self.from_dot = from_dot;
        self
    }

    /// The absolute value: the offset plus the section address for a
    /// relative value, the value itself otherwise.
    pub fn resolve<C: EvalContext<Section = S> + ?Sized>(&self, ctx: &C) -> u64 {
        match self.section {
            ValueSection::Relative(section) => self.value.wrapping_add(ctx.section_vma(section)),
            ValueSection::Number | ValueSection::Absolute => self.value,
        }
    }
}

/// Everything an expression can read, supplied by the caller.
///
/// Only [`EvalContext::Section`] and [`EvalContext::section_vma`] must be
/// provided. Lookup methods default to reporting the name as undefined, the
/// location counter to [`EvalError::NoLocationCounter`], and page sizes to
/// [`EvalError::NotYetKnown`], so a context can implement just what it has.
///
/// Section and region names are passed exactly as written. `ALIGNOF` and
/// `SIZEOF` may name `NEXT_SECTION`, meaning the next allocated output
/// section after the current one; contexts that support it recognize the
/// name.
pub trait EvalContext {
    /// Handle for an output section.
    type Section: Copy + Eq;

    /// The address (VMA) of an output section.
    fn section_vma(&self, section: Self::Section) -> u64;

    /// The output section whose description is being evaluated, or `None`
    /// outside output section descriptions.
    fn current_section(&self) -> Option<Self::Section> {
        None
    }

    /// The absolute value of the location counter.
    ///
    /// # Errors
    ///
    /// When `.` has no value here.
    fn dot(&self) -> Result<u64, EvalError> {
        Err(EvalError::NoLocationCounter)
    }

    /// GNU ld's `LD_FEATURE("SANE_EXPR")`: treat literals outside output
    /// sections as numbers rather than absolute addresses.
    fn sane_expr(&self) -> bool {
        false
    }

    /// The value of a symbol. A symbol defined absolutely should be returned
    /// as [`ValueSection::Absolute`]; the evaluator applies GNU ld's rule
    /// that such symbols act as numbers inside output sections.
    ///
    /// # Errors
    ///
    /// When the symbol is undefined or its value is not known.
    fn symbol(&mut self, name: &[u8]) -> Result<Value<Self::Section>, EvalError> {
        Err(EvalError::UndefinedSymbol(name.to_vec()))
    }

    /// `DEFINED(name)`.
    fn is_defined(&mut self, name: &[u8]) -> bool {
        let _ = name;
        false
    }

    /// `ADDR(name)`: normally `Value::relative(section, 0)`.
    ///
    /// # Errors
    ///
    /// When the section does not exist or has no address yet.
    fn section_addr(&mut self, name: &[u8]) -> Result<Value<Self::Section>, EvalError> {
        Err(EvalError::UndefinedSection(name.to_vec()))
    }

    /// `LOADADDR(name)`: the absolute load address.
    ///
    /// # Errors
    ///
    /// When the section does not exist or has no address yet.
    fn section_load_addr(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Err(EvalError::UndefinedSection(name.to_vec()))
    }

    /// `SIZEOF(name)`.
    ///
    /// # Errors
    ///
    /// When the section does not exist.
    fn section_size(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Err(EvalError::UndefinedSection(name.to_vec()))
    }

    /// `ALIGNOF(name)`.
    ///
    /// # Errors
    ///
    /// When the section does not exist.
    fn section_alignment(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Err(EvalError::UndefinedSection(name.to_vec()))
    }

    /// `ORIGIN(name)`.
    ///
    /// # Errors
    ///
    /// When the memory region does not exist.
    fn region_origin(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Err(EvalError::UndefinedRegion(name.to_vec()))
    }

    /// `LENGTH(name)`.
    ///
    /// # Errors
    ///
    /// When the memory region does not exist.
    fn region_length(&mut self, name: &[u8]) -> Result<u64, EvalError> {
        Err(EvalError::UndefinedRegion(name.to_vec()))
    }

    /// `SIZEOF_HEADERS`.
    ///
    /// # Errors
    ///
    /// When the header size is not known.
    fn sizeof_headers(&mut self) -> Result<u64, EvalError> {
        Err(EvalError::NotYetKnown("SIZEOF_HEADERS".into()))
    }

    /// `CONSTANT(MAXPAGESIZE)`.
    ///
    /// # Errors
    ///
    /// When the page size is not known.
    fn max_page_size(&self) -> Result<u64, EvalError> {
        Err(EvalError::NotYetKnown("MAXPAGESIZE".into()))
    }

    /// `CONSTANT(COMMONPAGESIZE)`.
    ///
    /// # Errors
    ///
    /// When the page size is not known.
    fn common_page_size(&self) -> Result<u64, EvalError> {
        Err(EvalError::NotYetKnown("COMMONPAGESIZE".into()))
    }

    /// `SEGMENT_START(name, default)`: the address given with
    /// `-T<name>` (`-Ttext-segment=...`), or `default`.
    fn segment_start(&mut self, name: &[u8], default: u64) -> u64 {
        let _ = name;
        default
    }

    /// `DATA_SEGMENT_ALIGN(maxpagesize, commonpagesize)` at location `dot`.
    ///
    /// The default is the manual's first form,
    /// `ALIGN(maxpagesize) + (. & (maxpagesize - 1))`. Layout overrides this
    /// to implement the `-z relro` adjustment.
    ///
    /// # Errors
    ///
    /// Implementations may fail when the value is not known yet.
    fn data_segment_align(
        &mut self,
        max_page_size: u64,
        common_page_size: u64,
        dot: u64,
    ) -> Result<u64, EvalError> {
        let _ = common_page_size;
        Ok(align_up(dot, max_page_size).wrapping_add(dot & max_page_size.wrapping_sub(1)))
    }

    /// `DATA_SEGMENT_RELRO_END(offset, value)`; the default returns `value`.
    ///
    /// # Errors
    ///
    /// Implementations may fail when the value is not known yet.
    fn data_segment_relro_end(&mut self, offset: u64, value: u64) -> Result<u64, EvalError> {
        let _ = offset;
        Ok(value)
    }

    /// `DATA_SEGMENT_END(value)`; the default returns `value`.
    ///
    /// # Errors
    ///
    /// Implementations may fail when the value is not known yet.
    fn data_segment_end(&mut self, value: u64) -> Result<u64, EvalError> {
        Ok(value)
    }

    /// Called when an `ASSERT` condition is zero. The default fails; a
    /// context evaluating before final layout can ignore it.
    ///
    /// # Errors
    ///
    /// [`EvalError::AssertionFailed`] by default.
    fn assertion_failed(&mut self, message: &[u8]) -> Result<(), EvalError> {
        Err(EvalError::AssertionFailed(message.to_vec()))
    }
}

/// GNU ld's `align_n`: `value` rounded up to a multiple of `align`, with
/// `align` 0 or 1 leaving it unchanged. The alignment need not be a power of
/// two. Wraps on overflow.
#[must_use]
pub fn align_up(value: u64, align: u64) -> u64 {
    if align <= 1 {
        return value;
    }
    value
        .wrapping_add(align.wrapping_sub(1))
        .checked_div(align)
        .unwrap_or(value)
        .wrapping_mul(align)
}

/// `LOG2CEIL(value)`: the smallest `n` with `2^n >= value`, and 0 for 0.
#[must_use]
pub fn log2_ceil(value: u64) -> u64 {
    if value <= 1 {
        0
    } else {
        64u64.saturating_sub(u64::from(value.wrapping_sub(1).leading_zeros()))
    }
}

/// Evaluates an expression.
///
/// Evaluation recurses once per tree level; trees produced by the parser are
/// limited in depth, so this cannot overflow the stack for parsed input.
///
/// # Errors
///
/// Undefined names, division by zero, failed assertions, and any error the
/// context reports.
pub fn eval<C: EvalContext + ?Sized>(
    expr: &Expr,
    ctx: &mut C,
) -> Result<Value<C::Section>, EvalError> {
    Evaluator { ctx }.eval(expr)
}

/// Evaluates an expression and returns its absolute value, as GNU ld does
/// for `MEMORY` origins and lengths and other contexts needing a plain
/// integer.
///
/// # Errors
///
/// As for [`eval`].
pub fn eval_absolute<C: EvalContext + ?Sized>(expr: &Expr, ctx: &mut C) -> Result<u64, EvalError> {
    let value = eval(expr, ctx)?;
    Ok(value.resolve(ctx))
}

/// Evaluates a symbol assignment and returns the symbol's new value.
///
/// Compound operators read the symbol's current value. A plain number
/// becomes relative to the current output section (or absolute outside
/// one), which is what GNU ld stores for the symbol. The `PROVIDE` rules
/// (define only if referenced and undefined) are the caller's.
///
/// # Errors
///
/// As for [`eval`]. Assigning `.` is not handled here: use
/// [`eval_dot_assignment`].
pub fn eval_symbol_assignment<C: EvalContext + ?Sized>(
    assignment: &Assignment,
    ctx: &mut C,
) -> Result<Value<C::Section>, EvalError> {
    let mut evaluator = Evaluator { ctx };
    let mut value = evaluator.assigned_value(assignment)?;
    if value.section == ValueSection::Number {
        value.section = match evaluator.ctx.current_section() {
            Some(section) => ValueSection::Relative(section),
            None => ValueSection::Absolute,
        };
    }
    Ok(value)
}

/// Evaluates an assignment to `.` and returns the new absolute location.
///
/// A plain number is an offset from the current output section's start.
///
/// # Errors
///
/// As for [`eval`], plus [`EvalError::DotBackwards`] when the location would
/// decrease inside an output section.
pub fn eval_dot_assignment<C: EvalContext + ?Sized>(
    assignment: &Assignment,
    ctx: &mut C,
) -> Result<u64, EvalError> {
    let mut evaluator = Evaluator { ctx };
    let value = evaluator.assigned_value(assignment)?;
    let ctx = evaluator.ctx;
    let current = ctx.current_section();
    let base = match value.section {
        ValueSection::Relative(section) => ctx.section_vma(section),
        ValueSection::Absolute => 0,
        ValueSection::Number => current.map_or(0, |section| ctx.section_vma(section)),
    };
    let next = value.value.wrapping_add(base);
    if current.is_some() {
        let dot = ctx.dot()?;
        if next < dot {
            return Err(EvalError::DotBackwards {
                from: dot,
                to: next,
            });
        }
    }
    Ok(next)
}

/// The bytes a fill expression repeats.
///
/// A bare hexadecimal literal gives one byte per two digits, most
/// significant first (`=0x9090` is `90 90`, and may be longer than eight
/// bytes). Any other expression gives its value's low 32 bits as four
/// big-endian bytes, as GNU ld does.
///
/// # Errors
///
/// As for [`eval`].
pub fn fill_pattern<C: EvalContext + ?Sized>(
    fill: &Fill,
    ctx: &mut C,
) -> Result<Vec<u8>, EvalError> {
    if let Some(digits) = &fill.hex_digits
        && !digits.is_empty()
    {
        let mut bytes = Vec::with_capacity(digits.len().div_ceil(2));
        // An odd digit count means an implied leading zero nibble.
        let mut pending: Option<u8> = (digits.len() & 1 == 1).then_some(0);
        for &d in digits {
            let nibble = (d as char)
                .to_digit(16)
                .and_then(|n| u8::try_from(n).ok())
                .unwrap_or(0);
            match pending.take() {
                Some(high) => bytes.push((high << 4) | nibble),
                None => pending = Some(nibble),
            }
        }
        return Ok(bytes);
    }
    let value = eval(&fill.expr, ctx)?.value;
    let low = u32::try_from(value & 0xffff_ffff).unwrap_or(0);
    Ok(low.to_be_bytes().to_vec())
}

/// Two operands brought to a common section, and the result's section.
type Reconciled<S> = (Value<S>, Value<S>, ValueSection<S>);

struct Evaluator<'c, C: ?Sized> {
    ctx: &'c mut C,
}

impl<C: EvalContext + ?Sized> Evaluator<'_, C> {
    fn outside_sections_insane(&self) -> bool {
        self.ctx.current_section().is_none() && !self.ctx.sane_expr()
    }

    /// A literal: absolute outside output sections, a number inside.
    fn literal(&self, value: u64) -> Value<C::Section> {
        if self.outside_sections_insane() {
            Value::absolute(value)
        } else {
            Value::number(value)
        }
    }

    /// GNU's `new_rel_from_abs`: an absolute address expressed relative to
    /// the current output section.
    fn rel_from_abs(&self, address: u64) -> Value<C::Section> {
        match self.ctx.current_section() {
            Some(section) => {
                Value::relative(section, address.wrapping_sub(self.ctx.section_vma(section)))
            }
            None => Value::absolute(address).with_from_dot(true),
        }
    }

    fn make_abs(&self, value: Value<C::Section>) -> Value<C::Section> {
        Value::absolute(value.resolve(self.ctx))
    }

    /// The `from_dot` flag two operands combine to.
    fn combined_from_dot(lhs: &Value<C::Section>, rhs: &Value<C::Section>) -> bool {
        lhs.from_dot || rhs.from_dot
    }

    fn symbol(&mut self, name: &[u8]) -> Result<Value<C::Section>, EvalError> {
        let mut value = self.ctx.symbol(name)?;
        if value.section == ValueSection::Absolute
            && (self.ctx.current_section().is_some() || self.ctx.sane_expr())
        {
            value.section = ValueSection::Number;
        }
        Ok(value)
    }

    fn assigned_value(&mut self, assignment: &Assignment) -> Result<Value<C::Section>, EvalError> {
        match assignment.op.binary() {
            None => self.eval(&assignment.expr),
            Some(op) => {
                let current = if assignment.is_dot() {
                    self.rel_from_abs(self.ctx.dot()?)
                } else {
                    self.symbol(&assignment.target)?
                };
                self.binary_values(op, current, &assignment.expr)
            }
        }
    }

    /// Evaluates the common, deeply nesting node kinds in a small frame
    /// and leaves the built-ins to [`Evaluator::eval_builtin`], so deep
    /// trees need little stack even in debug builds.
    fn eval(&mut self, expr: &Expr) -> Result<Value<C::Section>, EvalError> {
        match expr {
            Expr::Binary(op, lhs, rhs) => {
                let lhs = self.eval(lhs)?;
                self.binary_values(*op, lhs, rhs)
            }
            Expr::Unary(op, operand) => {
                let mut value = self.eval(operand)?;
                value.value = match op {
                    UnaryOp::Neg => value.value.wrapping_neg(),
                    UnaryOp::Not => u64::from(value.value == 0),
                    UnaryOp::BitNot => !value.value,
                };
                Ok(value)
            }
            Expr::Conditional(cond, then, otherwise) => {
                let branch = if self.eval(cond)?.value != 0 {
                    then
                } else {
                    otherwise
                };
                self.eval(branch)
            }
            _ => self.eval_builtin(expr),
        }
    }

    fn eval_builtin(&mut self, expr: &Expr) -> Result<Value<C::Section>, EvalError> {
        Ok(match expr {
            Expr::Number(value) => self.literal(*value),
            Expr::Symbol(name) => self.symbol(name)?,
            Expr::Dot => self.rel_from_abs(self.ctx.dot()?),
            Expr::Unary(op, operand) => {
                let mut value = self.eval(operand)?;
                value.value = match op {
                    UnaryOp::Neg => value.value.wrapping_neg(),
                    UnaryOp::Not => u64::from(value.value == 0),
                    UnaryOp::BitNot => !value.value,
                };
                value
            }
            Expr::Binary(op, lhs, rhs) => {
                let lhs = self.eval(lhs)?;
                self.binary_values(*op, lhs, rhs)?
            }
            Expr::Conditional(cond, then, otherwise) => {
                if self.eval(cond)?.value != 0 {
                    self.eval(then)?
                } else {
                    self.eval(otherwise)?
                }
            }
            Expr::Absolute(operand) => {
                let value = self.eval(operand)?;
                self.make_abs(value)
            }
            Expr::Align(align) | Expr::Block(align) => {
                let align = self.eval(align)?.value;
                let dot = self.ctx.dot()?;
                self.rel_from_abs(align_up(dot, align))
            }
            Expr::Next(align) => {
                let align = self.eval(align)?;
                let align = self.make_abs(align).value;
                Value::absolute(align_up(self.ctx.dot()?, align))
            }
            Expr::AlignExpr(value, align) => {
                let value = self.eval(value)?;
                let align = self.eval(align)?;
                let (value, align, section) = self.reconcile(value, align);
                Value {
                    value: align_up(value.value, align.value),
                    section,
                    from_dot: Self::combined_from_dot(&value, &align),
                }
            }
            Expr::Log2Ceil(operand) => Value::number(log2_ceil(self.eval(operand)?.value)),
            Expr::Max(a, b) | Expr::Min(a, b) => {
                let a = self.eval(a)?;
                let b = self.eval(b)?;
                let (a, b, section) = self.reconcile(a, b);
                let value = if matches!(expr, Expr::Max(..)) {
                    a.value.max(b.value)
                } else {
                    a.value.min(b.value)
                };
                Value {
                    value,
                    section,
                    from_dot: Self::combined_from_dot(&a, &b),
                }
            }
            Expr::DataSegmentAlign(max_page, common_page) => {
                let max_page = self.eval(max_page)?;
                let common_page = self.eval(common_page)?;
                let (max_page, common_page, _) = self.reconcile(max_page, common_page);
                let dot = self.ctx.dot()?;
                let value = self
                    .ctx
                    .data_segment_align(max_page.value, common_page.value, dot)?;
                Value::absolute(value)
            }
            Expr::DataSegmentRelroEnd(offset, value) => {
                let offset = self.eval(offset)?;
                let value = self.eval(value)?;
                let (value, offset, section) = self.reconcile(value, offset);
                let result = self.ctx.data_segment_relro_end(offset.value, value.value)?;
                Value {
                    value: result,
                    section,
                    from_dot: false,
                }
            }
            Expr::DataSegmentEnd(operand) => {
                let mut value = self.eval(operand)?;
                value.value = self.ctx.data_segment_end(value.value)?;
                value
            }
            Expr::SegmentStart(name, default) => {
                let default = self.eval(default)?.value;
                let address = self.ctx.segment_start(name, default);
                self.rel_from_abs(address)
            }
            Expr::Defined(name) => Value::number(u64::from(self.ctx.is_defined(name))),
            Expr::Addr(name) => self.ctx.section_addr(name)?,
            Expr::LoadAddr(name) => Value::absolute(self.ctx.section_load_addr(name)?),
            Expr::SizeOf(name) => Value::number(self.ctx.section_size(name)?),
            Expr::AlignOf(name) => Value::number(self.ctx.section_alignment(name)?),
            Expr::Length(name) => Value::number(self.ctx.region_length(name)?),
            Expr::Origin(name) => {
                let origin = self.ctx.region_origin(name)?;
                self.rel_from_abs(origin)
            }
            Expr::SizeOfHeaders => Value::number(self.ctx.sizeof_headers()?),
            Expr::Constant(name) => Value::number(match name.as_slice() {
                b"MAXPAGESIZE" => self.ctx.max_page_size()?,
                b"COMMONPAGESIZE" => self.ctx.common_page_size()?,
                _ => return Err(EvalError::UnknownConstant(name.clone())),
            }),
            Expr::Assert(condition, message) => {
                let value = self.eval(condition)?;
                if value.value == 0 {
                    self.ctx.assertion_failed(message)?;
                }
                value
            }
        })
    }

    /// Brings two operands to a common section, as GNU's `fold_binary`
    /// does, and returns them with the section of the result.
    ///
    /// Operands in different sections, neither a plain number, are both
    /// made absolute. A plain number takes the other operand's section.
    fn reconcile(
        &self,
        mut lhs: Value<C::Section>,
        mut rhs: Value<C::Section>,
    ) -> Reconciled<C::Section> {
        if lhs.section != rhs.section {
            if lhs.section != ValueSection::Number && rhs.section != ValueSection::Number {
                lhs = self.make_abs(lhs);
                rhs = self.make_abs(rhs);
            } else if rhs.section == ValueSection::Number {
                rhs.section = lhs.section;
                // Marks that one operand was a plain number.
                lhs.section = ValueSection::Number;
            }
        }
        let section = rhs.section;
        (lhs, rhs, section)
    }

    fn binary_values(
        &mut self,
        op: BinaryOp,
        lhs: Value<C::Section>,
        rhs: &Expr,
    ) -> Result<Value<C::Section>, EvalError> {
        match op {
            BinaryOp::LogicalAnd => {
                return Ok(Value::number(u64::from(
                    lhs.value != 0 && self.eval(rhs)?.value != 0,
                )));
            }
            BinaryOp::LogicalOr => {
                return Ok(Value::number(u64::from(
                    lhs.value != 0 || self.eval(rhs)?.value != 0,
                )));
            }
            _ => {}
        }
        let rhs = self.eval(rhs)?;
        let (lhs, rhs, section) = self.reconcile(lhs, rhs);
        let (l, r) = (lhs.value, rhs.value);
        let compare = |result: bool| Ok(Value::number(u64::from(result)));
        let value = match op {
            BinaryOp::Add => l.wrapping_add(r),
            BinaryOp::Sub => l.wrapping_sub(r),
            BinaryOp::Mul => l.wrapping_mul(r),
            BinaryOp::Shl => l.wrapping_shl(shift_count(r)),
            BinaryOp::Shr => l.wrapping_shr(shift_count(r)),
            BinaryOp::And => l & r,
            BinaryOp::Xor => l ^ r,
            BinaryOp::Or => l | r,
            BinaryOp::Div | BinaryOp::Rem => {
                if r == 0 {
                    return Err(EvalError::DivisionByZero);
                }
                let (sl, sr) = (l.cast_signed(), r.cast_signed());
                // `r` is non-zero, so these only fail for `i64::MIN / -1`,
                // where the wrapped results are `i64::MIN` and 0.
                let result = if op == BinaryOp::Div {
                    sl.checked_div(sr).unwrap_or(sl)
                } else {
                    sl.checked_rem(sr).unwrap_or(0)
                };
                result.cast_unsigned()
            }
            BinaryOp::Lt => return compare(l < r),
            BinaryOp::Gt => return compare(l > r),
            BinaryOp::Le => return compare(l <= r),
            BinaryOp::Ge => return compare(l >= r),
            BinaryOp::Eq => return compare(l == r),
            BinaryOp::Ne => return compare(l != r),
            BinaryOp::LogicalAnd => return compare(l != 0 && r != 0),
            BinaryOp::LogicalOr => return compare(l != 0 || r != 0),
        };
        // GNU's `arith_result_section`: an operation on two values of the
        // same section yields a number inside output sections and an
        // absolute value outside them.
        let section = if section == lhs.section {
            if self.outside_sections_insane() {
                ValueSection::Absolute
            } else {
                ValueSection::Number
            }
        } else {
            section
        };
        Ok(Value {
            value,
            section,
            from_dot: Self::combined_from_dot(&lhs, &rhs),
        })
    }
}

/// Shift counts are reduced modulo 64.
fn shift_count(count: u64) -> u32 {
    u32::try_from(count & 63).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::parse_expression;
    use std::path::Path;

    /// A context with two sections, `.text` at 0x1000 (id 0) and `.data` at
    /// 0x2000 (id 1).
    #[derive(Default)]
    struct Ctx {
        current: Option<u32>,
        dot: Option<u64>,
    }

    impl EvalContext for Ctx {
        type Section = u32;

        fn section_vma(&self, section: u32) -> u64 {
            [0x1000, 0x2000].get(section as usize).copied().unwrap_or(0)
        }

        fn current_section(&self) -> Option<u32> {
            self.current
        }

        fn dot(&self) -> Result<u64, EvalError> {
            self.dot.ok_or(EvalError::NoLocationCounter)
        }

        fn symbol(&mut self, name: &[u8]) -> Result<Value<u32>, EvalError> {
            match name {
                b"text_sym" => Ok(Value::relative(0, 0x10)),
                b"data_sym" => Ok(Value::relative(1, 0x20)),
                b"abs_sym" => Ok(Value::absolute(0x500)),
                _ => Err(EvalError::UndefinedSymbol(name.to_vec())),
            }
        }
    }

    fn run(src: &str, ctx: &mut Ctx) -> Result<Value<u32>, EvalError> {
        let expr = parse_expression(src.as_bytes(), Path::new("t")).expect("parse");
        eval(&expr, ctx)
    }

    fn abs(src: &str) -> u64 {
        let mut ctx = Ctx {
            dot: Some(0x1234),
            ..Ctx::default()
        };
        let value = run(src, &mut ctx).unwrap_or_else(|e| panic!("{src}: {e}"));
        value.resolve(&ctx)
    }

    #[test]
    fn arithmetic_matches_gnu() {
        assert_eq!(abs("-1 / 2"), 0);
        assert_eq!(abs("-1 % 3"), u64::MAX);
        assert_eq!(abs("1 << 65"), 2);
        assert_eq!(abs("5 - 7"), (-2i64).cast_unsigned());
        assert_eq!(abs("0x8000000000000000 / -1"), 0x8000_0000_0000_0000);
        assert_eq!(abs("7 & 3 == 3"), 1);
        assert_eq!(abs("1 | 6 ^ 3 & 5"), 7);
        assert_eq!(abs("~0 >> 60 << 1"), 0x1e);
        assert_eq!(abs("!5 + !0*16"), 16);
        assert_eq!(abs("0 ? 1 : 0 ? 3 : 4"), 4);
        assert_eq!(abs("1 ? 0 ? 7 : 8 : 9"), 8);
        assert_eq!(abs("3 - 2 - 1"), 0);
        assert_eq!(abs("2 * 3 % 4"), 2);
        assert_eq!(abs("1 << 2 + 1"), 8);
        assert_eq!(abs("3 && 4 | 1"), 1);
        assert_eq!(abs("0 || 2 && 3"), 1);
        assert_eq!(abs("- - 5"), 5);
        assert!(matches!(
            run("3 % 0", &mut Ctx::default()),
            Err(EvalError::DivisionByZero)
        ));
        assert!(matches!(
            run("3 / (1 - 1)", &mut Ctx::default()),
            Err(EvalError::DivisionByZero)
        ));
        // Short-circuit: the undefined symbol is never read.
        assert_eq!(abs("0 && nosuch"), 0);
        assert_eq!(abs("1 || nosuch"), 1);
        assert_eq!(abs("1 ? 2 : nosuch"), 2);
    }

    #[test]
    fn helpers() {
        assert_eq!(align_up(5, 4), 8);
        assert_eq!(align_up(5, 3), 6);
        assert_eq!(align_up(3, 0), 3);
        assert_eq!(align_up(u64::MAX, 16), 0);
        assert_eq!(log2_ceil(0), 0);
        assert_eq!(log2_ceil(1), 0);
        assert_eq!(log2_ceil(5), 3);
        assert_eq!(log2_ceil(4), 2);
        assert_eq!(log2_ceil(u64::MAX), 64);
    }

    #[test]
    fn section_rules() {
        // Outside sections, literals are absolute.
        let mut ctx = Ctx::default();
        assert_eq!(run("4", &mut ctx), Ok(Value::absolute(4)));
        // Outside output sections a literal is itself absolute, so adding it
        // to a relative value makes both absolute (GNU's rule).
        assert_eq!(run("text_sym + 4", &mut ctx), Ok(Value::absolute(0x1014)));
        // Different sections: both made absolute.
        assert_eq!(
            run("data_sym - text_sym", &mut ctx),
            Ok(Value::absolute(0x2020 - 0x1010))
        );
        // Same section outside output sections: absolute.
        assert_eq!(run("text_sym - text_sym", &mut ctx), Ok(Value::absolute(0)));
        // A value taken from `.` outside a section is flagged, so layout can
        // make the symbol relative to the section holding `.`.
        let mut ctx = Ctx {
            dot: Some(0x1080),
            ..Ctx::default()
        };
        assert_eq!(
            run("ALIGN(0x100)", &mut ctx),
            Ok(Value::absolute(0x1100).with_from_dot(true))
        );
        assert_eq!(
            run("ABSOLUTE(ALIGN(0x100))", &mut ctx),
            Ok(Value::absolute(0x1100))
        );
        // Inside an output section, literals and absolute symbols are numbers.
        let mut ctx = Ctx {
            current: Some(0),
            dot: Some(0x1008),
        };
        assert_eq!(run("4", &mut ctx), Ok(Value::number(4)));
        assert_eq!(run("text_sym + 4", &mut ctx), Ok(Value::relative(0, 0x14)));
        assert_eq!(run("abs_sym", &mut ctx), Ok(Value::number(0x500)));
        assert_eq!(run(".", &mut ctx), Ok(Value::relative(0, 8)));
        assert_eq!(run("ABSOLUTE(.)", &mut ctx), Ok(Value::absolute(0x1008)));
        assert_eq!(run("text_sym - text_sym", &mut ctx), Ok(Value::number(0)));
        assert_eq!(run("ALIGN(16)", &mut ctx), Ok(Value::relative(0, 0x10)));
    }

    #[test]
    fn assignments() {
        let parse = |s: &str| crate::script::parse_defsym(s.as_bytes()).expect("parse");
        let mut ctx = Ctx {
            current: Some(1),
            dot: Some(0x2010),
        };
        assert_eq!(
            eval_symbol_assignment(&parse("x = 0x10"), &mut ctx),
            Ok(Value::relative(1, 0x10))
        );
        assert_eq!(
            eval_symbol_assignment(&parse("text_sym += 1"), &mut ctx),
            Ok(Value::relative(0, 0x11))
        );
        assert_eq!(
            eval_dot_assignment(&parse(". = 0x20"), &mut ctx),
            Ok(0x2020)
        );
        assert_eq!(eval_dot_assignment(&parse(". += 8"), &mut ctx), Ok(0x2018));
        assert_eq!(
            eval_dot_assignment(&parse(". = 0x8"), &mut ctx),
            Err(EvalError::DotBackwards {
                from: 0x2010,
                to: 0x2008
            })
        );
        let mut ctx = Ctx {
            current: None,
            dot: Some(0x5000),
        };
        assert_eq!(
            eval_dot_assignment(&parse(". = 0x100"), &mut ctx),
            Ok(0x100)
        );
        assert_eq!(
            eval_symbol_assignment(&parse("y = 3"), &mut ctx),
            Ok(Value::absolute(3))
        );
    }

    #[test]
    fn fills() {
        let mut ctx = Ctx::default();
        let hex = Fill {
            expr: Expr::Number(0),
            hex_digits: Some(b"11223344556677889900".to_vec()),
        };
        assert_eq!(
            fill_pattern(&hex, &mut ctx).unwrap(),
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0x00]
        );
        let odd = Fill {
            expr: Expr::Number(0),
            hex_digits: Some(b"abc".to_vec()),
        };
        assert_eq!(fill_pattern(&odd, &mut ctx).unwrap(), [0x0a, 0xbc]);
        let plain = Fill {
            expr: Expr::Number(0x90),
            hex_digits: None,
        };
        assert_eq!(fill_pattern(&plain, &mut ctx).unwrap(), [0, 0, 0, 0x90]);
    }
}
