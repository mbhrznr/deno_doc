use super::render_context::RenderContext;
use super::util::*;
use crate::html::comrak_adapters::URLRewriter;
use crate::html::ShortPath;
use crate::js_doc::JsDoc;
use crate::js_doc::JsDocTag;
use crate::DocNodeKind;
use comrak::nodes::Ast;
use comrak::nodes::AstNode;
use comrak::nodes::NodeHtmlBlock;
use comrak::nodes::NodeValue;
use comrak::Arena;
use serde::Serialize;
use std::borrow::Cow;
use std::cell::RefCell;
use std::io::BufWriter;
use std::io::Write;

lazy_static! {
  static ref JSDOC_LINK_RE: regex::Regex = regex::Regex::new(
    r"(?m)\{\s*@link(?P<modifier>code|plain)?\s+(?P<value>[^}]+)}"
  )
  .unwrap();
  static ref LINK_RE: regex::Regex =
    regex::Regex::new(r"(^\.{0,2}\/)|(^[A-Za-z]+:\S)").unwrap();
  static ref MODULE_LINK_RE: regex::Regex =
    regex::Regex::new(r"^\[(\S+)\](?:\.(\S+)|\s|)$").unwrap();
}

lazy_static! {
  static ref AMMONIA: ammonia::Builder<'static> = {
    let mut ammonia_builder = ammonia::Builder::default();

    ammonia_builder
      .add_tags(["video", "button", "svg", "path", "rect"])
      .add_generic_attributes(["id", "align"])
      .add_tag_attributes("button", ["data-copy"])
      .add_tag_attributes(
        "svg",
        [
          "width",
          "height",
          "viewBox",
          "fill",
          "xmlns",
          "stroke",
          "stroke-width",
          "stroke-linecap",
          "stroke-linejoin",
        ],
      )
      .add_tag_attributes(
        "path",
        [
          "d",
          "fill",
          "fill-rule",
          "clip-rule",
          "stroke",
          "stroke-width",
          "stroke-linecap",
          "stroke-linejoin",
        ],
      )
      .add_tag_attributes("rect", ["x", "y", "width", "height", "fill"])
      .add_tag_attributes("video", ["src", "controls"])
      .add_allowed_classes("pre", ["highlight"])
      .add_allowed_classes("button", ["context_button"])
      .add_allowed_classes(
        "div",
        [
          "alert",
          "alert-note",
          "alert-tip",
          "alert-important",
          "alert-warning",
          "alert-caution",
        ],
      )
      .link_rel(Some("nofollow"))
      .url_relative(ammonia::UrlRelative::Custom(Box::new(
        AmmoniaRelativeUrlEvaluator(),
      )));

    #[cfg(feature = "syntect")]
    ammonia_builder.add_tag_attributes("span", ["style"]);

    #[cfg(feature = "tree-sitter")]
    ammonia_builder.add_allowed_classes("span", super::tree_sitter::CLASSES);

    ammonia_builder
  };
}

thread_local! {
  static CURRENT_FILE: RefCell<Option<Option<ShortPath>>> = const { RefCell::new(None) };
  static URL_REWRITER: RefCell<Option<Option<URLRewriter>>> = const { RefCell::new(None) };
}

fn parse_links<'a>(md: &'a str, ctx: &RenderContext) -> Cow<'a, str> {
  JSDOC_LINK_RE.replace_all(md, |captures: &regex::Captures| {
    let code = captures
      .name("modifier")
      .map_or("plain", |modifier_match| modifier_match.as_str())
      == "code";
    let value = captures.name("value").unwrap().as_str();

    let (link, mut title) = if let Some((link, title)) =
      value.split_once('|').or_else(|| value.split_once(' '))
    {
      (link.trim(), title.trim().to_string())
    } else {
      (value, "".to_string())
    };

    let link = if let Some(module_link_captures) = MODULE_LINK_RE.captures(link)
    {
      let module_match = module_link_captures.get(1).unwrap();
      let module_link = module_match.as_str();
      let symbol_match = module_link_captures.get(2);

      let mut link = link.to_string();

      let module = ctx.ctx.doc_nodes.iter().find(|(short_path, _)| {
        short_path.path == module_link
          || short_path.display_name() == module_link
      });

      if let Some((short_path, nodes)) = module {
        if let Some(symbol_match) = symbol_match {
          if nodes
            .iter()
            .any(|node| node.get_qualified_name() == symbol_match.as_str())
          {
            link = ctx.ctx.resolve_path(
              ctx.get_current_resolve(),
              UrlResolveKind::Symbol {
                file: short_path,
                symbol: symbol_match.as_str(),
              },
            );
            if title.is_empty() {
              title = format!(
                "{} {}",
                short_path.display_name(),
                symbol_match.as_str()
              );
            }
          }
        } else {
          link = ctx.ctx.resolve_path(
            ctx.get_current_resolve(),
            short_path.as_resolve_kind(),
          );
          if title.is_empty() {
            title = short_path.display_name().to_string();
          }
        }
      } else if let Some((external_link, external_title)) =
        ctx.ctx.href_resolver.resolve_external_jsdoc_module(
          module_link,
          symbol_match.map(|symbol_match| symbol_match.as_str()),
        )
      {
        link = external_link;
        title = external_title;
      }

      link
    } else {
      link.to_string()
    };

    let (title, link) = if let Some(href) = ctx.lookup_symbol_href(&link) {
      let title = if title.is_empty() {
        link
      } else {
        title.to_string()
      };

      (title, href)
    } else {
      let title = if title.is_empty() {
        link.clone()
      } else {
        title.to_string()
      };

      (title, link)
    };

    if LINK_RE.is_match(&link) {
      if code {
        format!("[`{title}`]({link})")
      } else {
        format!("[{title}]({link})")
      }
    } else {
      #[allow(clippy::collapsible_if)]
      if code {
        format!("`{title}`")
      } else {
        title.to_string()
      }
    }
  })
}

fn split_markdown_title(md: &str) -> (Option<&str>, Option<&str>) {
  let newline = md.find("\n\n").unwrap_or(usize::MAX);
  let codeblock = md.find("```").unwrap_or(usize::MAX);

  let index = newline.min(codeblock).min(md.len());

  match md.split_at(index) {
    ("", body) => (None, Some(body)),
    (title, "") => (None, Some(title)),
    (title, body) => (Some(title), Some(body)),
  }
}

struct AmmoniaRelativeUrlEvaluator();

impl<'b> ammonia::UrlRelativeEvaluate<'b> for AmmoniaRelativeUrlEvaluator {
  fn evaluate<'a>(&self, url: &'a str) -> Option<Cow<'a, str>> {
    URL_REWRITER.with(|url_rewriter| {
      if let Some(url_rewriter) = url_rewriter.borrow().as_ref().unwrap() {
        CURRENT_FILE.with(|current_file| {
          Some(
            url_rewriter(current_file.borrow().as_ref().unwrap().as_ref(), url)
              .into(),
          )
        })
      } else {
        Some(Cow::Borrowed(url))
      }
    })
  }
}

enum Alert {
  Note,
  Tip,
  Important,
  Warning,
  Caution,
}

fn match_node_value<'a>(
  arena: &'a Arena<AstNode<'a>>,
  node: &'a AstNode<'a>,
  options: &comrak::Options,
  plugins: &comrak::Plugins,
) {
  match &node.data.borrow().value {
    NodeValue::BlockQuote => {
      if let Some(paragraph_child) = node.first_child() {
        if paragraph_child.data.borrow().value == NodeValue::Paragraph {
          let alert = paragraph_child.first_child().and_then(|text_child| {
            if let NodeValue::Text(text) = &text_child.data.borrow().value {
              match text
                .split_once(' ')
                .map_or((text.as_str(), None), |(kind, title)| {
                  (kind, Some(title))
                }) {
                ("[!NOTE]", title) => {
                  Some((Alert::Note, title.unwrap_or("Note").to_string()))
                }
                ("[!TIP]", title) => {
                  Some((Alert::Tip, title.unwrap_or("Tip").to_string()))
                }
                ("[!IMPORTANT]", title) => Some((
                  Alert::Important,
                  title.unwrap_or("Important").to_string(),
                )),
                ("[!WARNING]", title) => {
                  Some((Alert::Warning, title.unwrap_or("Warning").to_string()))
                }
                ("[!CAUTION]", title) => {
                  Some((Alert::Caution, title.unwrap_or("Caution").to_string()))
                }
                _ => None,
              }
            } else {
              None
            }
          });

          if let Some((alert, title)) = alert {
            let start_col = node.data.borrow().sourcepos.start;

            let document = arena.alloc(AstNode::new(RefCell::new(Ast::new(
              NodeValue::Document,
              start_col,
            ))));

            let node_without_alert = arena.alloc(AstNode::new(RefCell::new(
              Ast::new(NodeValue::Paragraph, start_col),
            )));

            for child_node in paragraph_child.children().skip(1) {
              node_without_alert.append(child_node);
            }
            for child_node in node.children().skip(1) {
              node_without_alert.append(child_node);
            }

            document.append(node_without_alert);

            let html = render_node(document, options, plugins);

            let alert_title = match alert {
              Alert::Note => format!(
                "{}{title}",
                include_str!("./templates/icons/info-circle.svg")
              ),
              Alert::Tip => {
                format!("{}{title}", include_str!("./templates/icons/bulb.svg"))
              }
              Alert::Important => format!(
                "{}{title}",
                include_str!("./templates/icons/warning-message.svg")
              ),
              Alert::Warning => format!(
                "{}{title}",
                include_str!("./templates/icons/warning-triangle.svg")
              ),
              Alert::Caution => format!(
                "{}{title}",
                include_str!("./templates/icons/warning-octagon.svg")
              ),
            };

            let html = format!(
              r#"<div class="alert alert-{}"><div>{alert_title}</div><div>{html}</div></div>"#,
              match alert {
                Alert::Note => "note",
                Alert::Tip => "tip",
                Alert::Important => "important",
                Alert::Warning => "warning",
                Alert::Caution => "caution",
              }
            );

            let alert_node = arena.alloc(AstNode::new(RefCell::new(Ast::new(
              NodeValue::HtmlBlock(NodeHtmlBlock {
                block_type: 6,
                literal: html,
              }),
              start_col,
            ))));
            node.insert_before(alert_node);
            node.detach();
          }
        }
      }
    }
    NodeValue::Link(link) => {
      if link.url.ends_with(".mov") || link.url.ends_with(".mp4") {
        let start_col = node.data.borrow().sourcepos.start;

        let html = format!(r#"<video src="{}" controls></video>"#, link.url);

        let alert_node = arena.alloc(AstNode::new(RefCell::new(Ast::new(
          NodeValue::HtmlBlock(NodeHtmlBlock {
            block_type: 6,
            literal: html,
          }),
          start_col,
        ))));
        node.insert_before(alert_node);
        node.detach();
      }
    }
    _ => {}
  }
}

fn walk_node<'a>(
  arena: &'a Arena<AstNode<'a>>,
  node: &'a AstNode<'a>,
  options: &comrak::Options,
  plugins: &comrak::Plugins,
) {
  for child in node.children() {
    match_node_value(arena, child, options, plugins);
    walk_node(arena, child, options, plugins);
  }
}

fn walk_node_title<'a>(node: &'a AstNode<'a>) {
  for child in node.children() {
    if matches!(
      child.data.borrow().value,
      NodeValue::Document
        | NodeValue::Paragraph
        | NodeValue::Heading(_)
        | NodeValue::Text(_)
        | NodeValue::Code(_)
        | NodeValue::HtmlInline(_)
        | NodeValue::Emph
        | NodeValue::Strong
        | NodeValue::Strikethrough
        | NodeValue::Superscript
        | NodeValue::Link(_)
        | NodeValue::Math(_)
        | NodeValue::Escaped
        | NodeValue::WikiLink(_)
        | NodeValue::Underline
        | NodeValue::SoftBreak
    ) {
      walk_node_title(child);
    } else {
      // delete the node
      child.detach();
    }
  }
}

fn render_node<'a>(
  node: &'a AstNode<'a>,
  options: &comrak::Options,
  plugins: &comrak::Plugins,
) -> String {
  let mut bw = BufWriter::new(Vec::new());
  comrak::format_html_with_plugins(node, options, &mut bw, plugins).unwrap();
  String::from_utf8(bw.into_inner().unwrap()).unwrap()
}

pub struct MarkdownToHTMLOptions {
  pub title_only: bool,
  pub no_toc: bool,
}

pub fn strip(render_ctx: &RenderContext, md: &str) -> String {
  let mut options = comrak::Options::default();
  options.extension.autolink = true;
  options.extension.description_lists = true;
  options.extension.strikethrough = true;
  options.extension.superscript = true;
  options.extension.table = true;
  options.extension.tagfilter = true;
  options.extension.tasklist = true;
  options.render.escape = true;

  let md = parse_links(md, render_ctx);

  let arena = Arena::new();
  let root = comrak::parse_document(&arena, &md, &options);

  walk_node(&arena, root, &options, &Default::default());

  fn collect_text<'a>(node: &'a AstNode<'a>, output: &mut BufWriter<Vec<u8>>) {
    match node.data.borrow().value {
      NodeValue::Text(ref literal)
      | NodeValue::Code(comrak::nodes::NodeCode { ref literal, .. }) => {
        output.write_all(literal.as_bytes()).unwrap();
      }
      NodeValue::LineBreak | NodeValue::SoftBreak => {
        output.write_all(&[b' ']).unwrap()
      }
      _ => {
        for n in node.children() {
          collect_text(n, output);
        }
      }
    }
  }

  let mut bw = BufWriter::new(Vec::new());
  collect_text(root, &mut bw);
  String::from_utf8(bw.into_inner().unwrap()).unwrap()
}

pub fn markdown_to_html(
  render_ctx: &RenderContext,
  md: &str,
  render_options: MarkdownToHTMLOptions,
) -> Option<String> {
  // TODO(bartlomieju): this should be initialized only once
  let mut options = comrak::Options::default();
  options.extension.autolink = true;
  options.extension.description_lists = true;
  options.extension.strikethrough = true;
  options.extension.superscript = true;
  options.extension.table = true;
  options.extension.tagfilter = true;
  options.extension.tasklist = true;
  options.render.unsafe_ = true; // its fine because we run ammonia afterwards

  let mut plugins = comrak::Plugins::default();

  if !render_options.title_only {
    plugins.render.codefence_syntax_highlighter =
      Some(&render_ctx.ctx.highlight_adapter);
    if !render_options.no_toc {
      plugins.render.heading_adapter = Some(&render_ctx.toc);
    }
  }

  let md = parse_links(md, render_ctx);

  let class_name = if render_options.title_only {
    "markdown_summary"
  } else {
    "markdown"
  };

  let html = {
    let arena = Arena::new();
    let root = comrak::parse_document(&arena, &md, &options);

    if render_options.title_only {
      walk_node_title(root);

      if let Some(child) = root.first_child() {
        render_node(child, &options, &plugins)
      } else {
        return None;
      }
    } else {
      walk_node(&arena, root, &options, &plugins);
      render_node(root, &options, &plugins)
    }
  };

  CURRENT_FILE.set(Some(render_ctx.get_current_resolve().get_file().cloned()));
  URL_REWRITER.set(Some(render_ctx.ctx.url_rewriter.clone()));

  let html = Some(format!(
    r#"<div class="{class_name}">{}</div>"#,
    AMMONIA.clean(&html)
  ));

  CURRENT_FILE.set(None);
  URL_REWRITER.set(None);

  html
}

pub(crate) fn render_markdown(
  render_ctx: &RenderContext,
  md: &str,
  no_toc: bool,
) -> String {
  markdown_to_html(
    render_ctx,
    md,
    MarkdownToHTMLOptions {
      title_only: false,
      no_toc,
    },
  )
  .unwrap_or_default()
}

pub(crate) fn jsdoc_body_to_html(
  ctx: &RenderContext,
  js_doc: &JsDoc,
  summary: bool,
) -> Option<String> {
  if let Some(doc) = js_doc.doc.as_deref() {
    markdown_to_html(
      ctx,
      doc,
      MarkdownToHTMLOptions {
        title_only: summary,
        no_toc: false,
      },
    )
  } else {
    None
  }
}

pub(crate) fn jsdoc_examples(
  ctx: &RenderContext,
  js_doc: &JsDoc,
) -> Option<SectionCtx> {
  let mut i = 0;

  let examples = js_doc
    .tags
    .iter()
    .filter_map(|tag| {
      if let JsDocTag::Example { doc } = tag {
        let example = ExampleCtx::new(ctx, doc, i);
        i += 1;
        Some(example)
      } else {
        None
      }
    })
    .collect::<Vec<ExampleCtx>>();

  if !examples.is_empty() {
    Some(SectionCtx::new(
      ctx,
      "Examples",
      SectionContentCtx::Example(examples),
    ))
  } else {
    None
  }
}

#[derive(Debug, Serialize, Clone)]
pub struct ExampleCtx {
  pub anchor: AnchorCtx,
  pub id: String,
  pub title: String,
  pub markdown_title: String,
  markdown_body: String,
}

impl ExampleCtx {
  pub const TEMPLATE: &'static str = "example";

  pub fn new(render_ctx: &RenderContext, example: &str, i: usize) -> Self {
    let id = name_to_id("example", &i.to_string());

    let (maybe_title, body) = split_markdown_title(example);
    let title = if let Some(title) = maybe_title {
      title.to_string()
    } else {
      format!("Example {}", i + 1)
    };

    let markdown_title = render_markdown(render_ctx, &title, false);
    let markdown_body =
      render_markdown(render_ctx, body.unwrap_or_default(), true);

    ExampleCtx {
      anchor: AnchorCtx { id: id.to_string() },
      id: id.to_string(),
      title,
      markdown_title,
      markdown_body,
    }
  }
}

#[derive(Debug, Serialize, Clone, Default)]
pub struct ModuleDocCtx {
  pub deprecated: Option<String>,
  pub sections: super::SymbolContentCtx,
}

impl ModuleDocCtx {
  pub const TEMPLATE: &'static str = "module_doc";

  pub fn new(render_ctx: &RenderContext, short_path: &ShortPath) -> Self {
    let module_doc_nodes = render_ctx.ctx.doc_nodes.get(short_path).unwrap();

    let mut sections = Vec::with_capacity(7);

    let (deprecated, html) = if let Some(node) = module_doc_nodes
      .iter()
      .find(|n| n.kind() == DocNodeKind::ModuleDoc)
    {
      let deprecated = node.js_doc.tags.iter().find_map(|tag| {
        if let JsDocTag::Deprecated { doc } = tag {
          Some(render_markdown(
            render_ctx,
            doc.as_deref().unwrap_or_default(),
            false,
          ))
        } else {
          None
        }
      });

      if let Some(examples) = jsdoc_examples(render_ctx, &node.js_doc) {
        sections.push(examples);
      }

      let html = jsdoc_body_to_html(render_ctx, &node.js_doc, false);

      (deprecated, html)
    } else {
      (None, None)
    };

    if !short_path.is_main {
      let partitions_by_kind = super::partition::partition_nodes_by_kind(
        module_doc_nodes.iter().map(Cow::Borrowed),
        true,
      );

      sections.extend(super::namespace::render_namespace(
        partitions_by_kind.into_iter().map(|(title, nodes)| {
          (
            render_ctx.clone(),
            SectionHeaderCtx {
              title: title.clone(),
              anchor: AnchorCtx { id: title },
              href: None,
              doc: None,
            },
            nodes,
          )
        }),
      ));
    }

    Self {
      deprecated,
      sections: super::SymbolContentCtx {
        id: "module_doc".to_string(),
        docs: html,
        sections,
      },
    }
  }
}

#[cfg(test)]
mod test {
  use crate::html::href_path_resolve;
  use crate::html::jsdoc::parse_links;
  use crate::html::GenerateCtx;
  use crate::html::GenerateOptions;
  use crate::html::HrefResolver;
  use crate::DocNode;
  use crate::Location;
  use deno_ast::ModuleSpecifier;
  use indexmap::IndexMap;

  use crate::html::RenderContext;
  use crate::html::UrlResolveKind;
  use crate::interface::InterfaceDef;
  use crate::js_doc::JsDoc;
  use crate::node::DeclarationKind;

  struct EmptyResolver {}

  impl HrefResolver for EmptyResolver {
    fn resolve_path(
      &self,
      current: UrlResolveKind,
      target: UrlResolveKind,
    ) -> String {
      href_path_resolve(current, target)
    }

    fn resolve_global_symbol(&self, _symbol: &[String]) -> Option<String> {
      None
    }

    fn resolve_import_href(
      &self,
      _symbol: &[String],
      _src: &str,
    ) -> Option<String> {
      None
    }

    fn resolve_usage(&self, current_resolve: UrlResolveKind) -> Option<String> {
      current_resolve
        .get_file()
        .map(|current_file| current_file.display_name().to_string())
    }

    fn resolve_source(&self, _location: &Location) -> Option<String> {
      None
    }

    fn resolve_external_jsdoc_module(
      &self,
      _module: &str,
      _symbol: Option<&str>,
    ) -> Option<(String, String)> {
      None
    }
  }

  #[test]
  fn parse_links_test() {
    let ctx = GenerateCtx::new(
      GenerateOptions {
        package_name: None,
        main_entrypoint: None,
        href_resolver: std::rc::Rc::new(EmptyResolver {}),
        usage_composer: None,
        rewrite_map: None,
        category_docs: None,
        disable_search: false,
        symbol_redirect_map: None,
        default_symbol_map: None,
      },
      Default::default(),
      Default::default(),
      IndexMap::from([
        (
          ModuleSpecifier::parse("file:///a.ts").unwrap(),
          vec![
            DocNode::interface(
              "foo".into(),
              false,
              Location::default(),
              DeclarationKind::Export,
              JsDoc::default(),
              InterfaceDef {
                def_name: None,
                extends: vec![],
                constructors: vec![],
                methods: vec![],
                properties: vec![],
                call_signatures: vec![],
                index_signatures: vec![],
                type_params: Box::new([]),
              },
            ),
            DocNode::interface(
              "bar".into(),
              false,
              Location::default(),
              DeclarationKind::Export,
              JsDoc::default(),
              InterfaceDef {
                def_name: None,
                extends: vec![],
                constructors: vec![],
                methods: vec![],
                properties: vec![],
                call_signatures: vec![],
                index_signatures: vec![],
                type_params: Box::new([]),
              },
            ),
          ],
        ),
        (
          ModuleSpecifier::parse("file:///b.ts").unwrap(),
          vec![DocNode::interface(
            "baz".into(),
            false,
            Location::default(),
            DeclarationKind::Export,
            JsDoc::default(),
            InterfaceDef {
              def_name: None,
              extends: vec![],
              constructors: vec![],
              methods: vec![],
              properties: vec![],
              call_signatures: vec![],
              index_signatures: vec![],
              type_params: Box::new([]),
            },
          )],
        ),
      ]),
    )
    .unwrap();

    let (a_short_path, nodes) = ctx.doc_nodes.first().unwrap();

    let render_ctx = RenderContext::new(
      &ctx,
      nodes,
      UrlResolveKind::Symbol {
        file: a_short_path,
        symbol: "foo",
      },
    );

    assert_eq!(
      parse_links("foo {@link https://example.com} bar", &render_ctx),
      "foo [https://example.com](https://example.com) bar"
    );
    assert_eq!(
      parse_links("foo {@linkcode https://example.com} bar", &render_ctx),
      "foo [`https://example.com`](https://example.com) bar"
    );

    assert_eq!(
      parse_links("foo {@link https://example.com Example} bar", &render_ctx),
      "foo [Example](https://example.com) bar"
    );
    assert_eq!(
      parse_links("foo {@link https://example.com|Example} bar", &render_ctx),
      "foo [Example](https://example.com) bar"
    );
    assert_eq!(
      parse_links(
        "foo {@linkcode https://example.com Example} bar",
        &render_ctx
      ),
      "foo [`Example`](https://example.com) bar"
    );

    assert_eq!(
      parse_links("foo {@link unknownSymbol} bar", &render_ctx),
      "foo unknownSymbol bar"
    );
    assert_eq!(
      parse_links("foo {@linkcode unknownSymbol} bar", &render_ctx),
      "foo `unknownSymbol` bar"
    );

    #[cfg(not(target_os = "windows"))]
    {
      assert_eq!(
        parse_links("foo {@link bar} bar", &render_ctx),
        "foo [bar](../../.././/a.ts/~/bar.html) bar"
      );
      assert_eq!(
        parse_links("foo {@linkcode bar} bar", &render_ctx),
        "foo [`bar`](../../.././/a.ts/~/bar.html) bar"
      );

      assert_eq!(
        parse_links("foo {@link [b.ts]} bar", &render_ctx),
        "foo [b.ts](../../.././/b.ts/index.html) bar"
      );
      assert_eq!(
        parse_links("foo {@linkcode [b.ts]} bar", &render_ctx),
        "foo [`b.ts`](../../.././/b.ts/index.html) bar"
      );

      assert_eq!(
        parse_links("foo {@link [b.ts].baz} bar", &render_ctx),
        "foo [b.ts baz](../../.././/b.ts/~/baz.html) bar"
      );
      assert_eq!(
        parse_links("foo {@linkcode [b.ts].baz} bar", &render_ctx),
        "foo [`b.ts baz`](../../.././/b.ts/~/baz.html) bar"
      );
    }
  }

  #[test]
  fn markdown_alerts() {
    let ctx = GenerateCtx::new(
      GenerateOptions {
        package_name: None,
        main_entrypoint: None,
        href_resolver: std::rc::Rc::new(EmptyResolver {}),
        usage_composer: None,
        rewrite_map: None,
        category_docs: None,
        disable_search: false,
        symbol_redirect_map: None,
        default_symbol_map: None,
      },
      Default::default(),
      Default::default(),
      Default::default(),
    )
    .unwrap();

    let render_ctx = RenderContext::new(&ctx, &[], UrlResolveKind::AllSymbols);

    let md = super::render_markdown(
      &render_ctx,
      r#"
      > [!NOTE]
      > foo
      >
      > bar"#,
      true,
    );

    assert!(md.contains("foo"));
    assert!(md.contains("bar"));

    let md = super::render_markdown(
      &render_ctx,
      r#"
      > [!NOTE]
      >
      > foo
      >
      > bar"#,
      true,
    );

    assert!(md.contains("foo"));
    assert!(md.contains("bar"));
  }
}
