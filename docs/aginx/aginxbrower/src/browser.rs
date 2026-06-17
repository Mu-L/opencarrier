use crate::{ClickRequest, ClickResponse, EvalRequest, EvalResponse, FetchRequest, FetchResponse, OutputFormat};
use anyhow::Context;

/// Build an Obscura browser instance.
/// Supports `OBSCURA_PROXY` env var for SOCKS5/HTTP proxy.
pub fn build_browser() -> anyhow::Result<obscura::Browser> {
    let mut builder = obscura::Browser::builder().stealth(true);
    if let Ok(proxy) = std::env::var("OBSCURA_PROXY") {
        builder = builder.proxy(&proxy);
    }
    Ok(builder.build()?)
}

/// Fetch a page and return content in the requested format.
pub fn do_fetch(req: FetchRequest) -> anyhow::Result<FetchResponse> {
    let rt = tokio::runtime::Handle::try_current()
        .unwrap_or_else(|_| tokio::runtime::Handle::current());
    let _guard = rt.enter();

    let browser = build_browser()?;
    let mut page = rt.block_on(browser.new_page())?;
    rt.block_on(page.goto(&req.url))?;

    if let Some(wait) = req.wait_secs {
        rt.block_on(page.settle(wait * 1000));
    }

    let html = page.content();
    let title = page.evaluate("document.title").as_str().map(|s| s.to_string());

    let content = match req.format {
        OutputFormat::Html => html,
        OutputFormat::Text => extract_text(&html, req.selector.as_deref()),
        OutputFormat::Markdown => html_to_markdown(&html, req.selector.as_deref()),
    };

    Ok(FetchResponse {
        url: page.url(),
        title,
        content,
    })
}

/// Click an element by CSS selector using JS `element.click()`.
pub fn do_click(req: ClickRequest) -> anyhow::Result<ClickResponse> {
    let rt = tokio::runtime::Handle::try_current()
        .unwrap_or_else(|_| tokio::runtime::Handle::current());
    let _guard = rt.enter();

    let browser = build_browser()?;
    let mut page = rt.block_on(browser.new_page())?;
    rt.block_on(page.goto(&req.url))?;

    if let Some(wait) = req.wait_secs {
        rt.block_on(page.settle(wait * 1000));
    }

    let clicked = if let Some(el) = page.query_selector(&req.selector) {
        el.click().context("element.click() failed")?;
        true
    } else {
        false
    };

    rt.block_on(page.settle(500));
    let text_after = page.evaluate("document.body.innerText").as_str().map(|s| s.to_string());

    Ok(ClickResponse {
        url: page.url(),
        selector: req.selector,
        clicked,
        text_after,
    })
}

/// Evaluate arbitrary JavaScript on the page.
pub fn do_eval(req: EvalRequest) -> anyhow::Result<EvalResponse> {
    let rt = tokio::runtime::Handle::try_current()
        .unwrap_or_else(|_| tokio::runtime::Handle::current());
    let _guard = rt.enter();

    let browser = build_browser()?;
    let mut page = rt.block_on(browser.new_page())?;
    rt.block_on(page.goto(&req.url))?;

    if let Some(wait) = req.wait_secs {
        rt.block_on(page.settle(wait * 1000));
    }

    let result = page.evaluate(&req.script);

    Ok(EvalResponse {
        url: page.url(),
        result,
    })
}

fn extract_text(html: &str, selector: Option<&str>) -> String {
    let fragment = scraper::Html::parse_document(html);
    let selector = selector.and_then(|s| scraper::Selector::parse(s).ok());

    if let Some(sel) = selector {
        fragment
            .select(&sel)
            .map(|el| el.text().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        fragment.root_element().text().collect::<Vec<_>>().join(" ")
    }
}

fn html_to_markdown(html: &str, selector: Option<&str>) -> String {
    let fragment = scraper::Html::parse_document(html);
    let selector = selector.and_then(|s| scraper::Selector::parse(s).ok());
    let node_ref = selector
        .and_then(|sel| fragment.select(&sel).next())
        .map(|el| el.clone())
        .unwrap_or_else(|| fragment.root_element().clone());

    html2md::parse_html(&node_ref.html())
}
