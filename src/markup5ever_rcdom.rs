// Copyright 2014-2017 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A simple reference-counted DOM.
//!
//! This is sufficient as a static parse tree, but don't build a
//! web browser using it. :)
//!
//! A DOM is a [tree structure] with ordered children that can be represented in an XML-like
//! format. For example, the following graph
//!
//! ```text
//! div
//!  +- "text node"
//!  +- span
//! ```
//! in HTML would be serialized as
//!
//! ```html
//! <div>text node<span></span></div>
//! ```
//!
//! See the [document object model article on wikipedia][dom wiki] for more information.
//!
//! This implementation stores the information associated with each node once, and then hands out
//! refs to children. The nodes themselves are reference-counted to avoid copying - you can create
//! a new ref and then a node will outlive the document. Nodes own their children, but only have
//! weak references to their parents.
//!
//! [tree structure]: https://en.wikipedia.org/wiki/Tree_(data_structure)
//! [dom wiki]: https://en.wikipedia.org/wiki/Document_Object_Model

extern crate tendril;

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::io;
use std::mem;
use std::rc::{Rc, Weak};

use html5ever::interface::ElemName;
use tendril::StrTendril;

use html5ever::interface::tree_builder;
use html5ever::interface::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::serialize::TraversalScope;
use html5ever::serialize::TraversalScope::{ChildrenOnly, IncludeNode};
use html5ever::serialize::{Serialize, Serializer};
use html5ever::Attribute;
use html5ever::ExpandedName;
use html5ever::QualName;

/// The different kinds of nodes in the DOM.
#[derive(Debug)]
pub enum NodeData {
    /// The `Document` itself - the root node of a HTML document.
    Document,

    /// A `DOCTYPE` with name, public id, and system id. See
    /// [document type declaration on wikipedia][dtd wiki].
    ///
    /// [dtd wiki]: https://en.wikipedia.org/wiki/Document_type_declaration
    Doctype {
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    },

    /// A text node.
    Text { contents: RefCell<StrTendril> },

    /// A comment.
    Comment {
        /// The comment text.
        contents: StrTendril,
    },

    /// An element with attributes.
    Element {
        /// The qualified element name
        name: QualName,
        /// The element's attributes.
        attrs: RefCell<Vec<Attribute>>,

        /// For HTML \<template\> elements, the [template contents].
        ///
        /// [template contents]: https://html.spec.whatwg.org/multipage/#template-contents
        template_contents: RefCell<Option<Handle>>,

        /// Whether the node is a [HTML integration point].
        ///
        /// [HTML integration point]: https://html.spec.whatwg.org/multipage/#html-integration-point
        mathml_annotation_xml_integration_point: bool,
    },

    /// A Processing instruction.
    ProcessingInstruction {
        target: StrTendril,
        contents: StrTendril,
    },
}

/// A DOM node.
pub struct Node {
    /// Parent node.
    pub parent: Cell<Option<WeakHandle>>,
    /// Child nodes of this node.
    pub children: RefCell<Vec<Handle>>,
    /// Represents this node's data.
    pub data: NodeData,
}

impl Node {
    /// Create a new node from its contents
    pub fn new(data: NodeData) -> Rc<Self> {
        Rc::new(Node {
            data,
            parent: Cell::new(None),
            children: RefCell::new(Vec::new()),
        })
    }

    pub fn get_parent(&self) -> Option<Rc<Self>> {
        if let Some(parent) = self.parent.take() {
            let parent_handle = parent.upgrade();
            self.parent.set(Some(parent));
            parent_handle
        } else {
            None
        }
    }

    /// Return the nth child element of this node, or None.
    pub fn nth_child(&self, idx: usize) -> Option<Rc<Self>> {
        let mut element_idx = 0;
        for child in self.children.borrow().iter() {
            if let NodeData::Element { .. } = child.data {
                element_idx += 1;
                if element_idx == idx {
                    return Some(child.clone());
                }
            }
        }
        None
    }

    /// Return the element type (if an element)
    pub fn element_name(&self) -> Option<String> {
        if let NodeData::Element { ref name, .. } = self.data {
            Some(format!("{}", name.local_name()))
        } else {
            None
        }
    }

    /// Serialise the node to a writable.
    pub fn serialize(self: &Rc<Self>, writer: impl io::Write) -> io::Result<()> {
        html5ever::serialize(
            writer,
            &SerializableHandle(self.clone()),
            html5ever::serialize::SerializeOpts {
                scripting_enabled: true,
                traversal_scope: html5ever::serialize::TraversalScope::IncludeNode,
                create_missing_parent: false,
            },
        )
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let mut nodes = mem::take(&mut *self.children.borrow_mut());
        while let Some(node) = nodes.pop() {
            let children = mem::take(&mut *node.children.borrow_mut());
            nodes.extend(children.into_iter());
            if let NodeData::Element {
                ref template_contents,
                ..
            } = node.data
            {
                if let Some(template_contents) = template_contents.borrow_mut().take() {
                    nodes.push(template_contents);
                }
            }
        }
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Node")
            .field("data", &self.data)
            .field("children", &self.children)
            .finish()
    }
}

/// Reference to a DOM node.
pub type Handle = Rc<Node>;

/// Weak reference to a DOM node, used for parent pointers.
pub type WeakHandle = Weak<Node>;

/// Append a parentless node to another nodes' children
fn append(new_parent: &Handle, child: Handle) {
    let previous_parent = child.parent.replace(Some(Rc::downgrade(new_parent)));
    // Invariant: child cannot have existing parent
    assert!(previous_parent.is_none());
    new_parent.children.borrow_mut().push(child);
}

/// If the node has a parent, get it and this node's position in its children
fn get_parent_and_index(target: &Handle) -> Option<(Handle, usize)> {
    let weak = target.parent.take()?;
    let parent = weak.upgrade().expect("dangling weak pointer");
    target.parent.set(Some(weak));
    let i = match parent
        .children
        .borrow()
        .iter()
        .enumerate()
        .find(|&(_, child)| Rc::ptr_eq(child, target))
    {
        Some((i, _)) => i,
        None => panic!("have parent but couldn't find in parent's children!"),
    };
    Some((parent, i))
}

fn append_to_existing_text(prev: &Handle, text: &str) -> bool {
    match prev.data {
        NodeData::Text { ref contents } => {
            contents.borrow_mut().push_slice(text);
            true
        }
        _ => false,
    }
}

fn remove_from_parent(target: &Handle) {
    if let Some((parent, i)) = get_parent_and_index(target) {
        parent.children.borrow_mut().remove(i);
        target.parent.set(None);
    }
}

/// The DOM itself; the result of parsing.
pub struct RcDom {
    /// The `Document` itself.
    pub document: Handle,

    /// Errors that occurred during parsing.
    pub errors: RefCell<Vec<Cow<'static, str>>>,

    /// The document's quirks mode.
    pub quirks_mode: Cell<QuirksMode>,
}

impl RcDom {
    fn add_node_to_string(s: &mut String, node: &Handle, indent: usize) {
        use std::fmt::Write as _;
        match &node.data {
            NodeData::Document => {
                for child in &*node.children.borrow() {
                    Self::add_node_to_string(s, child, indent);
                }
            }
            NodeData::Doctype { .. } => {
                writeln!(s, "{0:indent$}<doctype>", "", indent = indent).unwrap();
            }
            NodeData::Text { contents } => {
                let borrowed = contents.borrow();
                let text = borrowed.to_string();
                if !text.trim().is_empty() {
                    writeln!(s, "{0:indent$}Text:{1}", "", text, indent = indent).unwrap();
                }
            }
            NodeData::Comment { .. } => (),
            NodeData::Element { name, .. } => {
                writeln!(s, "{0:indent$}<{1}>", "", name.local, indent = indent).unwrap();
                for child in &*node.children.borrow() {
                    Self::add_node_to_string(s, child, indent + 1);
                }
                writeln!(s, "{0:indent$}</{1}>", "", name.local, indent = indent).unwrap();
            }
            NodeData::ProcessingInstruction { .. } => {}
        }
    }

    /// A low-quality debug DOM rendering.
    pub fn as_dom_string(&self) -> String {
        let mut s = String::new();
        Self::add_node_to_string(&mut s, &self.document, 0);
        s
    }

    /// A low-quality debug DOM rendering of an individual node
    pub fn node_as_dom_string(node: &Handle) -> String {
        let mut s = String::new();
        Self::add_node_to_string(&mut s, node, 0);
        s
    }

    /// Serialise the DOM to a writable.
    pub fn serialize(&self, writer: impl io::Write) -> io::Result<()> {
        html5ever::serialize(
            writer,
            &SerializableHandle(self.document.clone()),
            html5ever::serialize::SerializeOpts {
                scripting_enabled: true,
                traversal_scope: html5ever::serialize::TraversalScope::IncludeNode,
                create_missing_parent: false,
            },
        )
    }

    /// Find the node at a child path starting from the root element.  At each level, 1 is the
    /// first child element, and only elements are counted.
    pub fn get_node_by_path(&self, path: &[usize]) -> Option<Handle> {
        let mut node = self.document.clone();
        for idx in path {
            node = match node.nth_child(*idx) {
                Some(new_node) => new_node,
                None => return None,
            };
        }
        Some(node)
    }
}

impl TreeSink for RcDom {
    type Output = Self;

    type ElemName<'a> = ExpandedName<'a>;
    fn finish(self) -> Self {
        self
    }

    type Handle = Handle;

    fn parse_error(&self, msg: Cow<'static, str>) {
        self.errors.borrow_mut().push(msg);
    }

    fn get_document(&self) -> Handle {
        self.document.clone()
    }

    fn get_template_contents(&self, target: &Handle) -> Handle {
        if let NodeData::Element {
            ref template_contents,
            ..
        } = target.data
        {
            template_contents
                .borrow()
                .as_ref()
                .expect("not a template element!")
                .clone()
        } else {
            panic!("not a template element!")
        }
    }

    fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks_mode.set(mode);
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        Rc::ptr_eq(x, y)
    }

    fn elem_name<'a>(&self, target: &'a Handle) -> ExpandedName<'a> {
        match target.data {
            NodeData::Element { ref name, .. } => name.expanded(),
            _ => panic!("not an element!"),
        }
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> Handle {
        Node::new(NodeData::Element {
            name,
            attrs: RefCell::new(attrs),
            template_contents: RefCell::new(if flags.template {
                Some(Node::new(NodeData::Document))
            } else {
                None
            }),
            mathml_annotation_xml_integration_point: flags.mathml_annotation_xml_integration_point,
        })
    }

    fn create_comment(&self, text: StrTendril) -> Handle {
        Node::new(NodeData::Comment { contents: text })
    }

    fn create_pi(&self, target: StrTendril, data: StrTendril) -> Handle {
        Node::new(NodeData::ProcessingInstruction {
            target,
            contents: data,
        })
    }

    fn append(&self, parent: &Handle, child: NodeOrText<Handle>) {
        // Append to an existing Text node if we have one.
        if let NodeOrText::AppendText(text) = &child {
            if let Some(h) = parent.children.borrow().last() {
                if append_to_existing_text(h, text) {
                    return;
                }
            }
        }

        append(
            parent,
            match child {
                NodeOrText::AppendText(text) => Node::new(NodeData::Text {
                    contents: RefCell::new(text),
                }),
                NodeOrText::AppendNode(node) => node,
            },
        );
    }

    fn append_before_sibling(&self, sibling: &Handle, child: NodeOrText<Handle>) {
        let (parent, i) = get_parent_and_index(sibling)
            .expect("append_before_sibling called on node without parent");

        let child = match (child, i) {
            // No previous node.
            (NodeOrText::AppendText(text), 0) => Node::new(NodeData::Text {
                contents: RefCell::new(text),
            }),

            // Look for a text node before the insertion point.
            (NodeOrText::AppendText(text), i) => {
                let children = parent.children.borrow();
                let prev = &children[i - 1];
                if append_to_existing_text(prev, &text) {
                    return;
                }
                Node::new(NodeData::Text {
                    contents: RefCell::new(text),
                })
            }

            // The tree builder promises we won't have a text node after
            // the insertion point.

            // Any other kind of node.
            (NodeOrText::AppendNode(node), _) => node,
        };

        remove_from_parent(&child);

        child.parent.set(Some(Rc::downgrade(&parent)));
        parent.children.borrow_mut().insert(i, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &Self::Handle,
        prev_element: &Self::Handle,
        child: NodeOrText<Self::Handle>,
    ) {
        let parent = element.parent.take();
        let has_parent = parent.is_some();
        element.parent.set(parent);

        if has_parent {
            self.append_before_sibling(element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        append(
            &self.document,
            Node::new(NodeData::Doctype {
                name,
                public_id,
                system_id,
            }),
        );
    }

    fn add_attrs_if_missing(&self, target: &Handle, attrs: Vec<Attribute>) {
        let mut existing = if let NodeData::Element { ref attrs, .. } = target.data {
            attrs.borrow_mut()
        } else {
            panic!("not an element")
        };

        let existing_names = existing
            .iter()
            .map(|e| e.name.clone())
            .collect::<HashSet<_>>();
        existing.extend(
            attrs
                .into_iter()
                .filter(|attr| !existing_names.contains(&attr.name)),
        );
    }

    fn remove_from_parent(&self, target: &Handle) {
        remove_from_parent(target);
    }

    fn reparent_children(&self, node: &Handle, new_parent: &Handle) {
        let mut children = node.children.borrow_mut();
        let mut new_children = new_parent.children.borrow_mut();
        for child in children.iter() {
            let previous_parent = child.parent.replace(Some(Rc::downgrade(new_parent)));
            assert!(Rc::ptr_eq(
                node,
                &previous_parent.unwrap().upgrade().expect("dangling weak")
            ))
        }
        new_children.extend(mem::take(&mut *children));
    }

    fn is_mathml_annotation_xml_integration_point(&self, target: &Handle) -> bool {
        if let NodeData::Element {
            mathml_annotation_xml_integration_point,
            ..
        } = target.data
        {
            mathml_annotation_xml_integration_point
        } else {
            panic!("not an element!")
        }
    }
}

impl Default for RcDom {
    fn default() -> RcDom {
        RcDom {
            document: Node::new(NodeData::Document),
            errors: vec![].into(),
            quirks_mode: tree_builder::NoQuirks.into(),
        }
    }
}

enum SerializeOp {
    Open(Handle),
    Close(QualName),
}

pub struct SerializableHandle(Handle);

impl From<Handle> for SerializableHandle {
    fn from(h: Handle) -> SerializableHandle {
        SerializableHandle(h)
    }
}

impl Serialize for SerializableHandle {
    fn serialize<S>(&self, serializer: &mut S, traversal_scope: TraversalScope) -> io::Result<()>
    where
        S: Serializer,
    {
        let mut ops = VecDeque::new();
        match traversal_scope {
            IncludeNode => ops.push_back(SerializeOp::Open(self.0.clone())),
            ChildrenOnly(_) => ops.extend(
                self.0
                    .children
                    .borrow()
                    .iter()
                    .map(|h| SerializeOp::Open(h.clone())),
            ),
        }

        while let Some(op) = ops.pop_front() {
            match op {
                SerializeOp::Open(handle) => match handle.data {
                    NodeData::Element {
                        ref name,
                        ref attrs,
                        ..
                    } => {
                        serializer.start_elem(
                            name.clone(),
                            attrs.borrow().iter().map(|at| (&at.name, &at.value[..])),
                        )?;

                        ops.reserve(1 + handle.children.borrow().len());
                        ops.push_front(SerializeOp::Close(name.clone()));

                        for child in handle.children.borrow().iter().rev() {
                            ops.push_front(SerializeOp::Open(child.clone()));
                        }
                    }

                    NodeData::Doctype { ref name, .. } => serializer.write_doctype(name)?,

                    NodeData::Text { ref contents } => serializer.write_text(&contents.borrow())?,

                    NodeData::Comment { ref contents } => serializer.write_comment(contents)?,

                    NodeData::ProcessingInstruction {
                        ref target,
                        ref contents,
                    } => serializer.write_processing_instruction(target, contents)?,

                    NodeData::Document => panic!("Can't serialize Document node itself"),
                },

                SerializeOp::Close(name) => {
                    serializer.end_elem(name)?;
                }
            }
        }

        Ok(())
    }
}
