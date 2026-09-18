//! The layout part of the link's linker scripts, flattened into one plan.
//!
//! Every script that affects layout (`-T`, `--default-script`, and implicit
//! scripts named as input files) is folded into a [`LayoutScript`]: the
//! top-level statement list GNU ld builds (symbol assignments, `ASSERT`s and
//! output section statements, in order), with `INSERT` blocks spliced in and
//! `OVERLAY`s expanded into output sections, plus `MEMORY`, `PHDRS` and the
//! other commands layout needs. Placement then adds orphan sections to the
//! statement list, and address assignment walks it.
//!
//! The plan owns clones of the syntax tree nodes it needs, with every
//! [`Span`] rebased onto [`LayoutScript::files`], so diagnostics name the
//! right file even when several scripts are involved.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::script::{
    Assert, AssignKind, Assignment, BinaryOp, CommandKind, DataSize, Expr, Fill,
    InputSectionDescription, InsertPosition, MemoryAttributes, OutputSection,
    OutputSectionCommandKind, OutputSectionType, Overlay, Phdr, Script, ScriptError,
    SectionConstraint, SectionsCommand, SectionsCommandKind, Span, VersionNode,
};

/// Most output section statements a plan may hold: output indices are
/// stored in 16 bits.
pub const MAX_OUTPUTS: usize = 60_000;

/// A top-level statement of the `SECTIONS` list.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    /// A symbol or location counter assignment outside output sections.
    Assign {
        /// The assignment.
        assignment: Assignment,
        /// Where it is.
        span: Span,
    },
    /// `ASSERT(expr, message)` outside output sections.
    Assert {
        /// The assertion.
        assert: Assert,
        /// Where it is.
        span: Span,
    },
    /// An output section statement, by index in [`LayoutScript::outputs`].
    Output(u32),
}

/// How an output section takes part in an `OVERLAY`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayRole {
    /// Not in an overlay.
    #[default]
    None,
    /// The first section of an overlay: its LMA comes from the overlay.
    First,
    /// A later section: its LMA follows the previous section's.
    Rest,
}

/// One statement inside an output section description.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// An input section description; `index` numbers the descriptions of
    /// the output section from 0, and is what placement records as the
    /// section's `sub` index.
    Input {
        /// The description.
        description: InputSectionDescription,
        /// Its number within the output section.
        index: u16,
    },
    /// A symbol or `.` assignment.
    Assign {
        /// The assignment.
        assignment: Assignment,
        /// Where it is.
        span: Span,
    },
    /// `BYTE`, `SHORT`, `LONG`, `QUAD` or `SQUAD`.
    Data {
        /// The width.
        size: DataSize,
        /// The value.
        expr: Expr,
        /// Where it is.
        span: Span,
    },
    /// `FILL(expr)`: the fill of later padding.
    Fill(Fill),
    /// `ASCIZ "string"`.
    Asciz(Vec<u8>),
    /// `LINKER_VERSION`: the linker's version string, NUL-terminated.
    LinkerVersion,
    /// `ASSERT(expr, message)`.
    Assert {
        /// The assertion.
        assert: Assert,
        /// Where it is.
        span: Span,
    },
}

/// An output section statement.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputStmt {
    /// The section name (`/DISCARD/` for discards).
    pub name: Vec<u8>,
    /// Where the statement starts.
    pub span: Span,
    /// The explicit start address.
    pub address: Option<Expr>,
    /// The `(type)`.
    pub section_type: OutputSectionType,
    /// `AT(lma)`.
    pub load_address: Option<Expr>,
    /// `ALIGN(expr)`.
    pub align: Option<Expr>,
    /// `ALIGN_WITH_INPUT`.
    pub align_with_input: bool,
    /// `SUBALIGN(expr)`.
    pub subalign: Option<Expr>,
    /// `ONLY_IF_RO`, `ONLY_IF_RW`, `SPECIAL`.
    pub constraint: SectionConstraint,
    /// The statements between the braces.
    pub items: Vec<Item>,
    /// `>region`.
    pub region: Option<Vec<u8>>,
    /// `AT>region`.
    pub load_region: Option<Vec<u8>>,
    /// `:phdr` names.
    pub phdrs: Vec<Vec<u8>>,
    /// `=fill`.
    pub fill: Option<Fill>,
    /// Overlay membership.
    pub overlay: OverlayRole,
    /// An assignment to `.` run after the section (the end of an overlay).
    pub update_dot: Option<Expr>,
}

impl OutputStmt {
    /// Whether this is `/DISCARD/`.
    #[must_use]
    pub fn is_discard(&self) -> bool {
        self.name == b"/DISCARD/"
    }

    /// Number of input section descriptions.
    #[must_use]
    pub fn input_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i, Item::Input { .. }))
            .count()
    }

    /// Whether the section type makes it not allocated (`INFO`, `COPY`,
    /// `DSECT`, `OVERLAY`).
    #[must_use]
    pub fn is_noalloc_type(&self) -> bool {
        matches!(
            self.section_type,
            OutputSectionType::Info
                | OutputSectionType::Copy
                | OutputSectionType::DSect
                | OutputSectionType::Overlay
        )
    }

    /// Whether the section is `NOLOAD`.
    #[must_use]
    pub fn is_noload(&self) -> bool {
        self.section_type == OutputSectionType::NoLoad
    }
}

/// A `MEMORY` region.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    /// Its name.
    pub name: Vec<u8>,
    /// Attributes, for sections without `>region`.
    pub attributes: MemoryAttributes,
    /// `ORIGIN`.
    pub origin: Expr,
    /// `LENGTH`.
    pub length: Expr,
    /// Where it is declared.
    pub span: Span,
}

/// `NOCROSSREFS` or `NOCROSSREFS_TO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoCrossRefs {
    /// `NOCROSSREFS_TO`: only references to the first section are checked.
    pub to: bool,
    /// Output section names.
    pub sections: Vec<Vec<u8>>,
}

/// The layout part of the link's scripts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutScript {
    /// Top-level statements, in order.
    pub statements: Vec<Statement>,
    /// Output section statements, by index.
    pub outputs: Vec<OutputStmt>,
    /// `MEMORY` regions, in declaration order.
    pub regions: Vec<Region>,
    /// `REGION_ALIAS(alias, region)` pairs.
    pub aliases: Vec<(Vec<u8>, Vec<u8>)>,
    /// `PHDRS`, when given.
    pub phdrs: Option<Vec<Phdr>>,
    /// Script files, indexed by [`Span::file`].
    pub files: Vec<PathBuf>,
    /// `LD_FEATURE("SANE_EXPR")`.
    pub sane_expr: bool,
    /// `NOCROSSREFS` commands.
    pub nocrossrefs: Vec<NoCrossRefs>,
    /// Whether the plan has output section statements of its own (from a
    /// `SECTIONS` command or the built-in default layout). Without, every
    /// section is an orphan.
    pub has_sections: bool,
    /// `VERSION` nodes found in the scripts.
    pub version: Vec<VersionNode>,
    /// Symbols the scripts assign outside `PROVIDE`, in order.
    pub defined: Vec<Vec<u8>>,
    /// Symbols only `PROVIDE`d, in order.
    pub provided: Vec<Vec<u8>>,
    /// Symbols the scripts read (for archive extraction and GC roots), and
    /// whether every read is on the right of a `PROVIDE`.
    pub referenced: Vec<(Vec<u8>, bool)>,
    /// The symbols of [`LayoutScript::referenced`] whose value is read, not
    /// only tested with `DEFINED` (which GNU ld does not count as a
    /// reference).
    pub value_reads: Vec<Vec<u8>>,
    /// `OVERWRITE_SECTIONS` descriptions, used for orphans of their name.
    pub overwrite: Vec<u32>,
}

impl LayoutScript {
    /// The file a span points into, for diagnostics.
    #[must_use]
    pub fn file_of(&self, span: Span) -> PathBuf {
        usize::try_from(span.file)
            .ok()
            .and_then(|i| self.files.get(i))
            .cloned()
            .unwrap_or_default()
    }

    /// An error at `span`.
    #[must_use]
    pub fn error_at(&self, span: Span, message: impl Into<String>) -> Error {
        Error::Script(Box::new(ScriptError::new(
            self.file_of(span),
            span.line,
            span.column,
            u64::from(span.offset),
            message,
        )))
    }

    /// The output statement with index `index`.
    #[must_use]
    pub fn output(&self, index: u32) -> Option<&OutputStmt> {
        self.outputs.get(index as usize)
    }

    /// The region named `name`, following `REGION_ALIAS`.
    #[must_use]
    pub fn region_index(&self, name: &[u8]) -> Option<usize> {
        let mut name = name;
        // Aliases may chain; a cycle stops after as many steps as aliases.
        for _ in 0..=self.aliases.len() {
            if let Some(index) = self.regions.iter().position(|r| r.name == name) {
                return Some(index);
            }
            match self.aliases.iter().find(|(alias, _)| alias == name) {
                Some((_, target)) => name = target,
                None => return None,
            }
        }
        None
    }

    /// The first output statement named `name`.
    #[must_use]
    pub fn find_output(&self, name: &[u8]) -> Option<u32> {
        self.statements.iter().find_map(|s| match s {
            Statement::Output(index) if self.output(*index).is_some_and(|o| o.name == name) => {
                Some(*index)
            }
            _ => None,
        })
    }
}

/// Collects scripts into a [`LayoutScript`].
#[derive(Debug, Default)]
pub struct Builder {
    plan: LayoutScript,
    /// Statements of `SECTIONS` commands not followed by `INSERT`.
    base: Vec<Statement>,
    /// `INSERT` blocks: position, anchor section, statements.
    inserts: Vec<(InsertPosition, Vec<u8>, Vec<Statement>, Span)>,
    /// `OVERWRITE_SECTIONS` statements.
    overwrite: Vec<Statement>,
    /// Whether any `SECTIONS` without `INSERT` was seen.
    saw_sections: bool,
}

impl Builder {
    /// Adds the layout commands of a parsed script. Commands that are not
    /// about layout (`INPUT`, `ENTRY`, ...) are ignored here.
    ///
    /// # Errors
    ///
    /// [`Error::Limit`] for too many output sections, script errors for
    /// malformed `OVERLAY`s.
    pub fn add(&mut self, script: &Script) -> Result<()> {
        let base = u32::try_from(self.plan.files.len())
            .map_err(|_| Error::Limit("too many linker scripts".into()))?;
        self.plan.files.extend(script.files.iter().cloned());
        let rebase = |span: Span| Span {
            file: span.file.saturating_add(base),
            ..span
        };
        let mut pending: Vec<Statement> = Vec::new();
        let mut pending_sections = false;
        for command in &script.commands {
            let span = rebase(command.span);
            match &command.kind {
                CommandKind::Sections(commands) => {
                    for c in commands {
                        self.sections_command(c, &rebase, &mut pending)?;
                    }
                    pending_sections = true;
                }
                CommandKind::OverwriteSections(commands) => {
                    let mut list = Vec::new();
                    for c in commands {
                        self.sections_command(c, &rebase, &mut list)?;
                    }
                    self.overwrite.extend(list);
                }
                CommandKind::Insert { position, section } => {
                    let block = std::mem::take(&mut pending);
                    pending_sections = false;
                    self.inserts.push((*position, section.clone(), block, span));
                }
                CommandKind::Assignment(assignment) => {
                    self.note_assignment(assignment);
                    pending.push(Statement::Assign {
                        assignment: assignment.clone(),
                        span,
                    });
                }
                CommandKind::Assert(assert) => {
                    self.note_expr(&assert.expr, false);
                    pending.push(Statement::Assert {
                        assert: assert.clone(),
                        span,
                    });
                }
                CommandKind::Memory(regions) => {
                    for region in regions {
                        self.plan.regions.push(Region {
                            name: region.name.clone(),
                            attributes: region.attributes,
                            origin: region.origin.clone(),
                            length: region.length.clone(),
                            span: rebase(region.span),
                        });
                    }
                }
                CommandKind::Phdrs(phdrs) => {
                    let list = self.plan.phdrs.get_or_insert_with(Vec::new);
                    list.extend(phdrs.iter().map(|p| Phdr {
                        span: rebase(p.span),
                        ..p.clone()
                    }));
                }
                CommandKind::RegionAlias { alias, region } => {
                    self.plan.aliases.push((alias.clone(), region.clone()));
                }
                CommandKind::NoCrossRefs(sections) => self.plan.nocrossrefs.push(NoCrossRefs {
                    to: false,
                    sections: sections.clone(),
                }),
                CommandKind::NoCrossRefsTo(sections) => self.plan.nocrossrefs.push(NoCrossRefs {
                    to: true,
                    sections: sections.clone(),
                }),
                CommandKind::LdFeature(name) => {
                    if name == b"SANE_EXPR" {
                        self.plan.sane_expr = true;
                    }
                }
                CommandKind::Version(nodes) => self.plan.version.extend(nodes.iter().cloned()),
                CommandKind::Entry(name) => self.note_name(name, false),
                CommandKind::Extern(names) => {
                    for name in names {
                        self.note_name(name, false);
                    }
                }
                _ => {}
            }
        }
        // Statements after the last INSERT (or all of them without one)
        // belong to the main list. Assignments outside SECTIONS count as
        // statements too: GNU ld adds them to the statement list in order.
        self.base.extend(pending);
        self.saw_sections |= pending_sections;
        if self.plan.outputs.len() > MAX_OUTPUTS {
            return Err(Error::Limit(format!(
                "more than {MAX_OUTPUTS} output section statements in linker scripts"
            )));
        }
        Ok(())
    }

    /// Whether the scripts replace the default layout (a `SECTIONS` command
    /// that is not inserted into the default one).
    #[must_use]
    pub fn replaces_default(&self) -> bool {
        self.saw_sections
    }

    /// Whether `INSERT` or `OVERWRITE_SECTIONS` was seen: such scripts
    /// augment the default layout rather than replace it.
    #[must_use]
    pub fn augments_default(&self) -> bool {
        !self.inserts.is_empty() || !self.overwrite.is_empty()
    }

    /// Whether the builder holds anything that affects layout.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
            && self.inserts.is_empty()
            && self.overwrite.is_empty()
            && self.plan.regions.is_empty()
            && self.plan.phdrs.is_none()
            && !self.saw_sections
    }

    /// Finishes the plan. When the built-in default layout is used, its
    /// script must have been added first.
    ///
    /// # Errors
    ///
    /// A script error when an `INSERT` names a section that does not exist.
    pub fn finish(mut self) -> Result<LayoutScript> {
        let mut statements = Vec::new();
        let has_sections = self.saw_sections;
        // The default statements come first, then the script's own.
        statements.extend(std::mem::take(&mut self.base));
        for (position, anchor, block, span) in std::mem::take(&mut self.inserts) {
            let at = statements.iter().position(|s| match s {
                Statement::Output(index) => self
                    .plan
                    .outputs
                    .get(*index as usize)
                    .is_some_and(|o| o.name == anchor),
                _ => false,
            });
            let Some(at) = at else {
                let file = usize::try_from(span.file)
                    .ok()
                    .and_then(|i| self.plan.files.get(i))
                    .cloned()
                    .unwrap_or_default();
                return Err(Error::Script(Box::new(ScriptError::new(
                    file,
                    span.line,
                    span.column,
                    u64::from(span.offset),
                    format!("{} not found for insert", String::from_utf8_lossy(&anchor)),
                ))));
            };
            let at = match position {
                InsertPosition::Before => at,
                InsertPosition::After => at.saturating_add(1),
            };
            let tail = statements.split_off(at);
            statements.extend(block);
            statements.extend(tail);
        }
        // OVERWRITE_SECTIONS: replace same-named outputs; the rest are used
        // for orphans of their name.
        for statement in std::mem::take(&mut self.overwrite) {
            let Statement::Output(index) = statement else {
                statements.push(statement);
                continue;
            };
            let Some(name) = self
                .plan
                .outputs
                .get(index as usize)
                .map(|o| o.name.clone())
            else {
                continue;
            };
            let slot = statements.iter_mut().find(|s| match s {
                Statement::Output(i) => self
                    .plan
                    .outputs
                    .get(*i as usize)
                    .is_some_and(|o| o.name == name),
                _ => false,
            });
            match slot {
                Some(slot) => *slot = Statement::Output(index),
                None => self.plan.overwrite.push(index),
            }
        }
        self.plan.statements = statements;
        self.plan.has_sections = has_sections;
        Ok(self.plan)
    }

    fn note_assignment(&mut self, assignment: &Assignment) {
        let provide = matches!(
            assignment.kind,
            AssignKind::Provide | AssignKind::ProvideHidden
        );
        if !assignment.is_dot() {
            if !provide && !self.plan.defined.contains(&assignment.target) {
                self.plan.defined.push(assignment.target.clone());
            }
            if provide && !self.plan.provided.contains(&assignment.target) {
                self.plan.provided.push(assignment.target.clone());
            }
        }
        if assignment.op.binary().is_some() && !assignment.is_dot() {
            self.note_name(&assignment.target, provide);
        }
        self.note_expr(&assignment.expr, provide);
    }

    fn push_output(&mut self, output: OutputStmt) -> Result<u32> {
        let index = u32::try_from(self.plan.outputs.len())
            .ok()
            .filter(|&i| (i as usize) < MAX_OUTPUTS)
            .ok_or_else(|| {
                Error::Limit(format!(
                    "more than {MAX_OUTPUTS} output section statements in linker scripts"
                ))
            })?;
        self.plan.outputs.push(output);
        Ok(index)
    }

    fn sections_command(
        &mut self,
        command: &SectionsCommand,
        rebase: &dyn Fn(Span) -> Span,
        out: &mut Vec<Statement>,
    ) -> Result<()> {
        let span = rebase(command.span);
        match &command.kind {
            SectionsCommandKind::Assignment(assignment) => {
                self.note_assignment(assignment);
                out.push(Statement::Assign {
                    assignment: assignment.clone(),
                    span,
                });
            }
            SectionsCommandKind::Entry(name) => {
                self.note_name(name, false);
            }
            SectionsCommandKind::Assert(assert) => {
                self.note_expr(&assert.expr, false);
                out.push(Statement::Assert {
                    assert: assert.clone(),
                    span,
                });
            }
            SectionsCommandKind::OutputSection(section) => {
                let output = self.output_stmt(section, span, rebase)?;
                let index = self.push_output(output)?;
                out.push(Statement::Output(index));
            }
            SectionsCommandKind::Overlay(overlay) => {
                self.overlay(overlay, span, rebase, out)?;
            }
        }
        Ok(())
    }

    fn items(
        &mut self,
        commands: &[crate::script::OutputSectionCommand],
        rebase: &dyn Fn(Span) -> Span,
    ) -> Result<Vec<Item>> {
        let mut items = Vec::with_capacity(commands.len());
        let mut inputs = 0u16;
        for command in commands {
            let span = rebase(command.span);
            items.push(match &command.kind {
                OutputSectionCommandKind::Assignment(assignment) => {
                    self.note_assignment(assignment);
                    Item::Assign {
                        assignment: assignment.clone(),
                        span,
                    }
                }
                OutputSectionCommandKind::Input(description) => {
                    let index = inputs;
                    inputs = inputs.checked_add(1).ok_or_else(|| {
                        Error::Limit(
                            "too many input section descriptions in one output section".into(),
                        )
                    })?;
                    Item::Input {
                        description: description.clone(),
                        index,
                    }
                }
                OutputSectionCommandKind::Data { size, expr } => {
                    self.note_expr(expr, false);
                    Item::Data {
                        size: *size,
                        expr: expr.clone(),
                        span,
                    }
                }
                OutputSectionCommandKind::Fill(fill) => Item::Fill(fill.clone()),
                OutputSectionCommandKind::Asciz(text) => Item::Asciz(text.clone()),
                OutputSectionCommandKind::LinkerVersion => Item::LinkerVersion,
                OutputSectionCommandKind::Assert(assert) => {
                    self.note_expr(&assert.expr, false);
                    Item::Assert {
                        assert: assert.clone(),
                        span,
                    }
                }
                // No-ops on ELF.
                OutputSectionCommandKind::CreateObjectSymbols
                | OutputSectionCommandKind::Constructors { .. } => continue,
            });
        }
        Ok(items)
    }

    fn output_stmt(
        &mut self,
        section: &OutputSection,
        span: Span,
        rebase: &dyn Fn(Span) -> Span,
    ) -> Result<OutputStmt> {
        for expr in [
            &section.address,
            &section.load_address,
            &section.align,
            &section.subalign,
        ]
        .into_iter()
        .flatten()
        {
            self.note_expr(expr, false);
        }
        Ok(OutputStmt {
            name: section.name.clone(),
            span,
            address: section.address.clone(),
            section_type: section.section_type.clone(),
            load_address: section.load_address.clone(),
            align: section.align.clone(),
            align_with_input: section.align_with_input,
            subalign: section.subalign.clone(),
            constraint: section.constraint,
            items: self.items(&section.commands, rebase)?,
            region: section.region.clone(),
            load_region: section.load_region.clone(),
            phdrs: section.phdrs.clone(),
            fill: section.fill.clone(),
            overlay: OverlayRole::None,
            update_dot: None,
        })
    }

    /// Expands an `OVERLAY` as GNU ld's `lang_leave_overlay` does: every
    /// section starts at the overlay's address; the first takes the
    /// overlay's `AT`, the others load after their predecessor; `.` ends at
    /// the start plus the largest size; `__load_start_NAME` and
    /// `__load_stop_NAME` are provided.
    fn overlay(
        &mut self,
        overlay: &Overlay,
        span: Span,
        rebase: &dyn Fn(Span) -> Span,
        out: &mut Vec<Statement>,
    ) -> Result<()> {
        let start = overlay.address.clone();
        let mut max_size: Option<Expr> = None;
        let mut names = Vec::new();
        for (position, section) in overlay.sections.iter().enumerate() {
            let size = Expr::SizeOf(section.name.clone());
            max_size = Some(match max_size {
                None => size,
                Some(previous) => Expr::Max(Box::new(previous), Box::new(size)),
            });
            let items = self.items(&section.commands, rebase)?;
            let output = OutputStmt {
                name: section.name.clone(),
                span: rebase(section.span),
                address: start.clone(),
                section_type: OutputSectionType::Normal,
                load_address: if position == 0 {
                    overlay.load_address.clone()
                } else {
                    None
                },
                align: None,
                align_with_input: false,
                subalign: overlay.subalign.clone(),
                constraint: SectionConstraint::None,
                items,
                region: overlay.region.clone(),
                load_region: overlay.load_region.clone(),
                phdrs: if section.phdrs.is_empty() {
                    overlay.phdrs.clone()
                } else {
                    section.phdrs.clone()
                },
                fill: section.fill.clone().or_else(|| overlay.fill.clone()),
                overlay: if position == 0 {
                    OverlayRole::First
                } else {
                    OverlayRole::Rest
                },
                update_dot: None,
            };
            let index = self.push_output(output)?;
            out.push(Statement::Output(index));
            names.push(section.name.clone());
            // The load address symbols.
            let clean: Vec<u8> = section
                .name
                .iter()
                .copied()
                .filter(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .collect();
            let load = || Expr::LoadAddr(section.name.clone());
            for (prefix, expr) in [
                (&b"__load_start_"[..], load()),
                (
                    &b"__load_stop_"[..],
                    Expr::Binary(
                        BinaryOp::Add,
                        Box::new(load()),
                        Box::new(Expr::SizeOf(section.name.clone())),
                    ),
                ),
            ] {
                let mut target = prefix.to_vec();
                target.extend_from_slice(&clean);
                out.push(Statement::Assign {
                    assignment: Assignment {
                        target,
                        op: crate::script::AssignOp::Assign,
                        expr,
                        kind: AssignKind::Provide,
                    },
                    span,
                });
            }
        }
        if overlay.no_cross_refs && names.len() > 1 {
            self.plan.nocrossrefs.push(NoCrossRefs {
                to: false,
                sections: names,
            });
        }
        // `.` after the overlay: start + largest size. It belongs to the
        // last section, so that it runs before the load symbols.
        let last = out
            .iter()
            .rev()
            .find_map(|s| match s {
                Statement::Output(index) => Some(*index),
                _ => None,
            })
            .and_then(|i| self.plan.outputs.get_mut(i as usize));
        if let (Some(last), Some(max_size)) = (last, max_size) {
            let base = start.unwrap_or(Expr::Addr(
                overlay
                    .sections
                    .first()
                    .map(|s| s.name.clone())
                    .unwrap_or_default(),
            ));
            last.update_dot = Some(Expr::Binary(
                BinaryOp::Add,
                Box::new(base),
                Box::new(max_size),
            ));
        }
        Ok(())
    }
}

impl Builder {
    fn note_name(&mut self, name: &[u8], provide: bool) {
        note_name(name, &mut self.plan.referenced, provide);
        if name != b"." && !self.plan.value_reads.iter().any(|n| n == name) {
            self.plan.value_reads.push(name.to_vec());
        }
    }

    fn note_expr(&mut self, expr: &Expr, provide: bool) {
        note_expr(expr, &mut self.plan.referenced, provide);
        let reads = &mut self.plan.value_reads;
        expr.for_each_value_symbol(&mut |name| {
            if !reads.iter().any(|n| n == name) {
                reads.push(name.to_vec());
            }
        });
    }
}

fn note_name(name: &[u8], list: &mut Vec<(Vec<u8>, bool)>, provide: bool) {
    if name == b"." {
        return;
    }
    match list.iter_mut().find(|(n, _)| n == name) {
        Some(entry) => entry.1 &= provide,
        None => list.push((name.to_vec(), provide)),
    }
}

fn note_expr(expr: &Expr, list: &mut Vec<(Vec<u8>, bool)>, provide: bool) {
    expr.for_each_symbol(&mut |name| note_name(name, list, provide));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{NoIncludes, parse_script};
    use std::path::Path;

    fn build(text: &str) -> LayoutScript {
        let script = parse_script(text.as_bytes(), Path::new("t.ld"), &mut NoIncludes).unwrap();
        let mut builder = Builder::default();
        builder.add(&script).unwrap();
        builder.finish().unwrap()
    }

    #[test]
    fn flattens_sections_and_overlays() {
        let plan = build(
            "MEMORY { rom (rx) : ORIGIN = 0, LENGTH = 64K }
             x = 1;
             SECTIONS {
               .text : { *(.text) y = .; }
               OVERLAY 0x1000 : AT (0x4000) { .a { *(.a) } .b { *(.b) } }
             }",
        );
        assert!(plan.has_sections);
        assert_eq!(plan.regions.len(), 1);
        assert_eq!(plan.outputs.len(), 3);
        assert!(matches!(
            plan.statements.first(),
            Some(Statement::Assign { .. })
        ));
        assert_eq!(plan.outputs[1].overlay, OverlayRole::First);
        assert_eq!(plan.outputs[2].overlay, OverlayRole::Rest);
        assert!(plan.outputs[2].update_dot.is_some());
        assert!(plan.defined.contains(&b"y".to_vec()));
        let names: Vec<_> = plan
            .statements
            .iter()
            .filter_map(|s| match s {
                Statement::Assign { assignment, .. } => Some(assignment.target.clone()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&b"__load_start_a".to_vec()));
    }

    #[test]
    fn inserts_into_the_base_list() {
        let base = build("SECTIONS { .text : { *(.text) } .data : { *(.data) } }");
        let script = parse_script(
            b"SECTIONS { .extra : { *(.extra) } } INSERT AFTER .text;",
            Path::new("i.ld"),
            &mut NoIncludes,
        )
        .unwrap();
        let default = parse_script(
            b"SECTIONS { .text : { *(.text) } .data : { *(.data) } }",
            Path::new("default"),
            &mut NoIncludes,
        )
        .unwrap();
        let mut builder = Builder::default();
        builder.add(&default).unwrap();
        builder.add(&script).unwrap();
        assert!(builder.augments_default());
        let plan = builder.finish().unwrap();
        assert_eq!(base.outputs.len(), 2);
        let names: Vec<_> = plan
            .statements
            .iter()
            .filter_map(|s| match s {
                Statement::Output(i) => Some(plan.outputs[*i as usize].name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            names,
            [b".text".to_vec(), b".extra".to_vec(), b".data".to_vec()]
        );
    }

    #[test]
    fn region_aliases_resolve() {
        let plan = build(
            "MEMORY { flash : ORIGIN = 0, LENGTH = 1K }
             REGION_ALIAS(\"rom\", flash) REGION_ALIAS(\"boot\", rom)",
        );
        assert_eq!(plan.region_index(b"boot"), Some(0));
        assert_eq!(plan.region_index(b"nope"), None);
    }
}
