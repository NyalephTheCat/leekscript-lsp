//! Syntax-tree scope for semantic tokens: type-expression regions vs value code, template lists.
//!
//! Each [`TokenScopeSite`] performs at most one covering-node lookup and one ancestor walk per token.

use leekscript::ast::{ClassDecl, FunctionDecl};
use leekscript::syntax::kinds::{Lex, Node};
use leekscript::visit::AstNodeTrait;
use sipha::tree::red::{SyntaxNode, SyntaxToken};
use sipha::types::{FromSyntaxKind, IntoSyntaxKind, SyntaxKind};

/// How to highlight an identifier that appears inside a type expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdentTypePosition {
    /// Name declared in an enclosing `function f<…>` or `class C<…>` template list.
    TemplateParameter,
    /// Ordinary type reference (custom or contextual type name).
    TypeName,
}

/// CST node kinds that wrap type syntax (custom names, unions, generics, template params, and
/// `instanceof Array<…>`-style built-in type expressions).
fn type_syntax_container_kind(k: SyntaxKind) -> bool {
    matches!(
        Node::from_syntax_kind(k),
        Some(
            Node::TypeExpr
                | Node::TypeUnionType
                | Node::TypeNullableType
                | Node::TypePrimaryType
                | Node::TemplateParams
                | Node::BuiltinTypeNameExpr
        )
    )
}

/// `true` when `decl` is a function or class that declares `name` in its `<…>` template list.
fn decl_template_lists_include_name(decl: &SyntaxNode, name: &str) -> bool {
    if let Some(fd) = FunctionDecl::cast(decl.clone()) {
        if let Some(tp) = fd.template_params() {
            if tp.names().iter().any(|s| s == name) {
                return true;
            }
        }
    }
    if let Some(cd) = ClassDecl::cast(decl.clone()) {
        if let Some(tp) = cd.template_params() {
            if tp.names().iter().any(|s| s == name) {
                return true;
            }
        }
    }
    false
}

/// Cached covering node and ancestor chain for one token (see module docs).
pub(crate) struct TokenScopeSite<'a> {
    token: &'a SyntaxToken,
    covering: SyntaxNode,
    ancestors: Vec<SyntaxNode>,
}

impl<'a> TokenScopeSite<'a> {
    #[must_use]
    pub fn new(root: &'a SyntaxNode, token: &'a SyntaxToken) -> Option<Self> {
        let covering = root.node_at_offset(token.offset())?;
        let ancestors = covering.ancestors(root);
        Some(Self {
            token,
            covering,
            ancestors,
        })
    }

    /// `true` when this token lies under a type-syntax subtree (not necessarily only under
    /// [`Node::TemplateParams`]).
    #[must_use]
    pub fn in_type_syntax(&self) -> bool {
        if type_syntax_container_kind(self.covering.kind()) {
            return true;
        }
        self.ancestors
            .iter()
            .any(|a| type_syntax_container_kind(a.kind()))
    }

    /// `true` when the token is inside a declaration template list (`function f<T>(…)`, `class C<T>`).
    #[must_use]
    pub fn in_template_params(&self) -> bool {
        let tpl = Node::TemplateParams.into_syntax_kind();
        if self.covering.kind() == tpl {
            return true;
        }
        self.ancestors.iter().any(|a| a.kind() == tpl)
    }

    /// For a [`Lex::Ident`] token: `true` when the name is declared as a template parameter on an
    /// enclosing function or class.
    #[must_use]
    pub fn ident_is_declared_template_param(&self) -> bool {
        if self.token.kind_as::<Lex>() != Some(Lex::Ident) {
            return false;
        }
        let name = self.token.text();
        if decl_template_lists_include_name(&self.covering, name) {
            return true;
        }
        self.ancestors
            .iter()
            .any(|a| decl_template_lists_include_name(a, name))
    }

    /// If this is an identifier in a type position, classify it for LSP semantic token types.
    #[must_use]
    pub fn classify_ident_in_type_position(&self) -> Option<IdentTypePosition> {
        if self.token.kind_as::<Lex>() != Some(Lex::Ident) {
            return None;
        }
        if !self.in_type_syntax() {
            return None;
        }
        if self.in_template_params() || self.ident_is_declared_template_param() {
            Some(IdentTypePosition::TemplateParameter)
        } else {
            Some(IdentTypePosition::TypeName)
        }
    }
}
