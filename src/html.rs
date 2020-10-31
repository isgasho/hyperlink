use std::fmt;
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;
use std::str;

use anyhow::Error;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::interner::StringInterner;
use crate::paragraph::ParagraphWalker;

static BAD_SCHEMAS: &[&str] = &[
    "http://", "https://", "irc://", "ftp://", "mailto:", "data:",
];

static PARAGRAPH_TAGS: &[&[u8]] = &[b"p", b"li", b"dt", b"dd"];

#[inline]
fn push_and_canonicalize(base: &mut String, path: &str) {
    if path.starts_with('/') {
        base.clear();
    } else if path.is_empty() {
        if base.ends_with('/') {
            base.truncate(base.len() - 1);
        }
        return;
    } else {
        base.truncate(base.rfind('/').unwrap_or(0));
    }

    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                base.truncate(base.rfind('/').unwrap_or(0));
            }
            _ => {
                if !base.is_empty() {
                    base.push('/');
                }
                base.push_str(component);
            }
        }
    }
}

#[test]
fn test_push_and_canonicalize() {
    let mut base = String::from("2019/");
    let path = "../feed.xml";
    push_and_canonicalize(&mut base, path);
    assert_eq!(base, "feed.xml");
}

#[test]
fn test_push_and_canonicalize2() {
    let mut base = String::from("contact.html");
    let path = "contact.html";
    push_and_canonicalize(&mut base, path);
    assert_eq!(base, "contact.html");
}

#[test]
fn test_push_and_canonicalize3() {
    let mut base = String::from("");
    let path = "./2014/article.html";
    push_and_canonicalize(&mut base, path);
    assert_eq!(base, "2014/article.html");
}

#[test]
fn test_push_and_canonicalize_empty_href() {
    let mut base = String::from("./foo/install.html");
    let path = "";
    push_and_canonicalize(&mut base, path);
    assert_eq!(base, "./foo/install.html");

    let mut base = String::from("./foo/");
    push_and_canonicalize(&mut base, path);
    assert_eq!(base, "./foo");
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct Href<'a>(&'a str);

impl<'a> Href<'a> {
    pub fn without_anchor(&self) -> Href<'_> {
        let mut s = &self.0[..];

        if let Some(i) = s.find('#') {
            s = &s[..i];
        }

        Href(s)
    }
}

impl<'a> fmt::Display for Href<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(fmt)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct UsedLink<'a, P> {
    pub href: Href<'a>,
    pub path: &'a Path,
    pub paragraph: Option<P>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct DefinedLink<'a> {
    pub href: Href<'a>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum Link<'a, P> {
    Uses(UsedLink<'a, P>),
    Defines(DefinedLink<'a>),
}

impl<'a, P> Link<'a, P> {
    pub fn into_paragraph(self) -> Option<P> {
        match self {
            Link::Uses(UsedLink { paragraph, .. }) => paragraph,
            Link::Defines(_) => None,
        }
    }
}

pub struct Document<'a> {
    pub path: &'a Path,
    pub href: Href<'a>,
    pub is_index_html: bool,
}

impl<'a> Document<'a> {
    pub fn new(interner: &'a StringInterner, base_path: &Path, path: &'a Path) -> Self {
        let mut href_path = path
            .strip_prefix(base_path)
            .expect("base_path is not a base of path");

        let is_index_html = href_path.ends_with("index.html") || href_path.ends_with("index.htm");

        if is_index_html {
            href_path = href_path.parent().unwrap_or(href_path);
        }

        let mut href = href_path
            .to_str()
            .expect("Invalid unicode in path")
            .to_owned();
        if cfg!(windows) {
            unsafe {
                // SAFETY: we replace ascii bytes only
                // SAFETY: href is an owned string
                let href = href.as_bytes_mut();
                for b in href.iter_mut() {
                    if *b == b'\\' {
                        *b = b'/';
                    }
                }
            }
        }

        let href = Href(&*interner.intern_string(&href));

        Document {
            path,
            href,
            is_index_html,
        }
    }

    fn join<'b>(
        &self,
        interner: &'b StringInterner,
        preserve_anchor: bool,
        rel_href: &str,
    ) -> Href<'b> {
        let qs_start = rel_href
            .find(&['?', '#'][..])
            .unwrap_or_else(|| rel_href.len());
        let anchor_start = rel_href.find('#').unwrap_or_else(|| rel_href.len());

        let mut href = self.href.0.to_owned();
        if self.is_index_html {
            href.push('/');
        }

        push_and_canonicalize(&mut href, &rel_href[..qs_start]);

        if preserve_anchor {
            let anchor = &rel_href[anchor_start..];
            if anchor.len() > 1 {
                href.push_str(anchor);
            }
        }

        Href(interner.intern_string(&href))
    }

    pub fn links<'b, 'link, P: ParagraphWalker>(
        &self,
        interner: &'b StringInterner,
        xml_buf: &mut Vec<u8>,
        sink: &mut Vec<Link<'link, P::Paragraph>>,
        check_anchors: bool,
        get_paragraphs: bool,
    ) -> Result<(), Error>
    where
        'a: 'link,
        'b: 'link,
    {
        self.links_from_read::<_, P>(
            interner,
            xml_buf,
            sink,
            fs::File::open(&self.path)?,
            check_anchors,
            get_paragraphs,
        )
    }

    fn links_from_read<'b, 'link, R: Read, P: ParagraphWalker>(
        &self,
        interner: &'b StringInterner,
        xml_buf: &mut Vec<u8>,
        sink: &mut Vec<Link<'link, P::Paragraph>>,
        read: R,
        check_anchors: bool,
        get_paragraphs: bool,
    ) -> Result<(), Error>
    where
        'a: 'link,
        'b: 'link,
    {
        let mut reader = Reader::from_reader(BufReader::new(read));
        reader.trim_text(true);
        reader.expand_empty_elements(true);
        reader.check_end_names(false);

        let mut paragraph_walker = P::new();

        let mut last_paragraph_i = sink.len();
        let mut in_paragraph = false;

        loop {
            match reader.read_event(xml_buf)? {
                Event::Eof => break,
                Event::Start(ref e) => {
                    if PARAGRAPH_TAGS.contains(&e.name()) {
                        in_paragraph = true;
                        last_paragraph_i = sink.len();
                        paragraph_walker.finish_paragraph();
                    }

                    macro_rules! extract_used_link {
                        ($attr_name:expr) => {
                            for attr in e.html_attributes() {
                                let attr = attr?;

                                if attr.key == $attr_name
                                    && BAD_SCHEMAS
                                        .iter()
                                        .all(|schema| !attr.value.starts_with(schema.as_bytes()))
                                {
                                    sink.push(Link::Uses(UsedLink {
                                        href: self.join(
                                            interner,
                                            check_anchors,
                                            str::from_utf8(&attr.unescaped_value()?)?,
                                        ),
                                        path: &self.path,
                                        paragraph: None,
                                    }));
                                }
                            }
                        };
                    }

                    macro_rules! extract_anchor_def {
                        ($attr_name:expr) => {
                            if check_anchors {
                                for attr in e.html_attributes() {
                                    let attr = attr?;

                                    if attr.key == $attr_name {
                                        let mut href = String::new();
                                        href.push('#');
                                        href.push_str(str::from_utf8(&attr.value)?);

                                        sink.push(Link::Defines(DefinedLink {
                                            href: self.join(
                                                interner,
                                                check_anchors,
                                                interner.intern_string(&href),
                                            ),
                                        }));
                                    }
                                }
                            }
                        };
                    }

                    match e.name() {
                        b"a" => {
                            extract_used_link!(b"href");
                            extract_anchor_def!(b"name");
                        }
                        b"img" => extract_used_link!(b"src"),
                        b"link" => extract_used_link!(b"href"),
                        b"script" => extract_used_link!(b"src"),
                        b"iframe" => extract_used_link!(b"src"),
                        b"area" => extract_used_link!(b"href"),
                        b"object" => extract_used_link!(b"data"),
                        _ => {}
                    }

                    extract_anchor_def!(b"id");
                }
                Event::End(e) if get_paragraphs => {
                    if PARAGRAPH_TAGS.contains(&e.name()) {
                        let paragraph = paragraph_walker.finish_paragraph();
                        if in_paragraph {
                            for link in &mut sink[last_paragraph_i..] {
                                match link {
                                    Link::Uses(ref mut x) => {
                                        x.paragraph = Some(paragraph.clone());
                                    }
                                    Link::Defines(_) => {}
                                }
                            }
                            in_paragraph = false;
                        }
                        last_paragraph_i = sink.len();
                    }
                }
                Event::Text(e) if get_paragraphs && in_paragraph => {
                    // XXX: Unescape properly https://github.com/tafia/quick-xml/issues/238
                    let text = e.unescaped().unwrap_or_else(|_| e.escaped().into());
                    paragraph_walker.update(str::from_utf8(&text)?);
                }
                _ => {}
            }
        }

        Ok(())
    }
}

#[test]
fn test_document_href() {
    let interner = StringInterner::default();
    let doc = Document::new(
        &interner,
        Path::new("public/"),
        Path::new("public/platforms/python/troubleshooting/index.html"),
    );

    assert_eq!(doc.href, Href("platforms/python/troubleshooting"));

    let doc = Document::new(
        &interner,
        Path::new("public/"),
        Path::new("public/platforms/python/troubleshooting.html"),
    );

    assert_eq!(doc.href, Href("platforms/python/troubleshooting.html"));
}

#[test]
fn test_document_links() {
    use crate::paragraph::ParagraphHasher;

    let interner = StringInterner::default();
    let doc = Document::new(
        &interner,
        Path::new("public/"),
        Path::new("public/platforms/python/troubleshooting/index.html"),
    );

    let mut links = Vec::new();

    doc.links_from_read::<_, ParagraphHasher>(
        &interner,
        &mut Vec::new(),
        &mut links,
        r#"""
    <a href="../../ruby/" />
    <a href="/platforms/perl/">Perl</a>

    <a href=../../rust/>
    <a href='../../go/'>
    """#
        .as_bytes(),
        false,
        false,
    )
    .unwrap();

    let used_link = |x: &'static str| {
        Link::Uses(UsedLink {
            href: Href(x),
            path: &doc.path,
            paragraph: None,
        })
    };

    assert_eq!(
        &links,
        &[
            used_link("platforms/ruby"),
            used_link("platforms/perl"),
            used_link("platforms/rust"),
            used_link("platforms/go"),
        ]
    );
}

#[test]
fn test_document_join_index_html() {
    let interner = StringInterner::default();
    let doc = Document::new(
        &interner,
        Path::new("public/"),
        Path::new("public/platforms/python/troubleshooting/index.html"),
    );

    assert_eq!(
        doc.join(&interner, false, "../../ruby#foo"),
        Href("platforms/ruby")
    );
    assert_eq!(
        doc.join(&interner, true, "../../ruby#foo"),
        Href("platforms/ruby#foo")
    );
    assert_eq!(
        doc.join(&interner, true, "../../ruby?bar=1#foo"),
        Href("platforms/ruby#foo")
    );

    assert_eq!(
        doc.join(&interner, false, "/platforms/ruby"),
        Href("platforms/ruby")
    );
    assert_eq!(
        doc.join(&interner, true, "/platforms/ruby?bar=1#foo"),
        Href("platforms/ruby#foo")
    );
}

#[test]
fn test_document_join_bare_html() {
    let interner = StringInterner::default();
    let doc = Document::new(
        &interner,
        Path::new("public/"),
        Path::new("public/platforms/python/troubleshooting.html"),
    );

    assert_eq!(
        doc.join(&interner, false, "../ruby#foo"),
        Href("platforms/ruby")
    );
    assert_eq!(
        doc.join(&interner, true, "../ruby#foo"),
        Href("platforms/ruby#foo")
    );
    assert_eq!(
        doc.join(&interner, true, "../ruby?bar=1#foo"),
        Href("platforms/ruby#foo")
    );

    assert_eq!(
        doc.join(&interner, false, "/platforms/ruby"),
        Href("platforms/ruby")
    );
    assert_eq!(
        doc.join(&interner, true, "/platforms/ruby?bar=1#foo"),
        Href("platforms/ruby#foo")
    );
}
