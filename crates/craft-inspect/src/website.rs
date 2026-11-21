//! Website study: screenshots, visible structure and computed visual tokens (Clean Room),
//! plus network, bundle and framework probes when Deep mode was explicitly authorized.

use crate::Probe;
use craft_browser::{Browser, BrowserError, Viewport, file_url};
use craft_core::{
    BehavioralRequirement, CleanRoomSpec, CraftError, CraftRequest, DeepFindings, EventSink,
    EvidenceKind, EvidenceStore, StudyReport, TargetLocation,
};
use serde_json::Value;
use uuid::Uuid;

/// Runs in the page. Reads only what a visitor can see: rendered text, roles, computed styles.
const OBSERVE_JS: &str = r#"(() => {
  const vis = el => { const r = el.getBoundingClientRect(), s = getComputedStyle(el);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none'; };
  const txt = el => (el.innerText || el.getAttribute('aria-label') || el.value || el.alt || '')
    .replace(/\s+/g, ' ').trim().slice(0, 120);
  const uniq = a => [...new Set(a.filter(Boolean))];
  const q = sel => [...document.querySelectorAll(sel)].filter(vis);
  const top = (m, n) => Object.entries(m).sort((a, b) => b[1] - a[1]).slice(0, n).map(e => e[0]);
  const clear = c => !c || c === 'transparent' || /rgba\(.*, 0\)$/.test(c);

  const backgrounds = {}, textColors = {}, fonts = {};
  for (const el of [...document.querySelectorAll('body *')].slice(0, 4000)) {
    if (!vis(el)) continue;
    const s = getComputedStyle(el), r = el.getBoundingClientRect();
    if (!clear(s.backgroundColor))
      backgrounds[s.backgroundColor] = (backgrounds[s.backgroundColor] || 0) + r.width * r.height;
    const own = [...el.childNodes].filter(n => n.nodeType === 3).map(n => n.textContent.trim()).join('').length;
    if (own) {
      textColors[s.color] = (textColors[s.color] || 0) + own;
      const family = s.fontFamily.split(',')[0].replace(/["']/g, '').trim();
      fonts[family] = (fonts[family] || 0) + own;
    }
  }
  const style = sel => { const el = q(sel)[0]; if (!el) return null; const s = getComputedStyle(el);
    return { font_family: s.fontFamily, font_size: s.fontSize, font_weight: s.fontWeight, color: s.color,
             background: clear(s.backgroundColor) ? null : s.backgroundColor, border_radius: s.borderRadius }; };
  const pageBg = [document.body, document.documentElement].map(e => getComputedStyle(e).backgroundColor)
    .find(c => !clear(c)) || 'rgb(255, 255, 255)';
  const meta = document.querySelector('meta[name="description"]');

  return {
    title: document.title,
    lang: document.documentElement.lang || '',
    description: meta ? meta.content : '',
    viewport: { width: innerWidth, height: innerHeight },
    page_height: document.documentElement.scrollHeight,
    landmarks: q('header,nav,main,footer,aside,section,form,[role=banner],[role=navigation],[role=main],[role=contentinfo],[role=search]')
      .slice(0, 40).map(el => { const r = el.getBoundingClientRect(), h = el.querySelector('h1,h2,h3');
        return { role: el.getAttribute('role') || el.tagName.toLowerCase(),
                 label: el.getAttribute('aria-label') || (h ? txt(h) : ''),
                 top: Math.round(r.top + scrollY), height: Math.round(r.height) }; }),
    headings: q('h1,h2,h3,h4').slice(0, 40).map(h => ({ level: +h.tagName[1], text: txt(h) })).filter(h => h.text),
    nav_links: uniq(q('nav a[href], header a[href], [role=navigation] a[href]').map(txt)).slice(0, 20),
    links: uniq(q('a[href]').map(txt)).slice(0, 40),
    buttons: uniq(q('button,[role=button],input[type=submit],input[type=button]').map(txt)).slice(0, 30),
    fields: q('input:not([type=hidden]):not([type=submit]):not([type=button]),select,textarea').slice(0, 30)
      .map(i => ({ type: i.type || i.tagName.toLowerCase(),
                   label: ((i.labels && i.labels[0]) ? txt(i.labels[0]) : '') || i.getAttribute('aria-label') || i.placeholder || '' })),
    images: q('img').slice(0, 30).map(i => { const r = i.getBoundingClientRect();
      return { alt: i.alt || '', width: Math.round(r.width), height: Math.round(r.height) }; }),
    visible_text: document.body.innerText.replace(/\s+/g, ' ').trim().slice(0, 3000),
    palette: { page_background: pageBg, backgrounds: top(backgrounds, 6), text: top(textColors, 6) },
    typography: { fonts: top(fonts, 4), body: style('body'), h1: style('h1'), h2: style('h2'),
                  link: style('a[href]'), button: style('button,[role=button]') },
  };
})()"#;

/// Deep mode only: what the page loads and what it is built with.
const DEEP_JS: &str = r#"(async () => {
  const uniq = a => [...new Set(a.filter(Boolean))];
  const resources = performance.getEntriesByType('resource')
    .map(r => ({ url: r.name.slice(0, 500), initiator: r.initiatorType, bytes: r.transferSize || 0 })).slice(0, 300);
  const scripts = uniq([...document.scripts].map(s => s.src));
  const sourcemaps = [];
  for (const src of scripts.slice(0, 20)) {
    try {
      if (new URL(src).origin !== location.origin) continue;
      const tail = (await (await fetch(src)).text()).slice(-600);
      const m = tail.match(/sourceMappingURL=([^\s*]+)/);
      if (m) sourcemaps.push(m[1].startsWith('data:') ? src + ' (inline source map)' : new URL(m[1], src).href);
    } catch (e) { /* unreadable script: nothing to report */ }
  }
  const has = n => typeof window[n] !== 'undefined';
  const sel = s => document.querySelector(s);
  const sample = [...document.querySelectorAll('body *')].slice(0, 300);
  const frameworks = [];
  if (has('__NEXT_DATA__') || sel('#__next')) frameworks.push('Next.js');
  if (has('__NUXT__') || sel('#__nuxt')) frameworks.push('Nuxt');
  if (has('React') || sel('[data-reactroot]') || sample.some(el => Object.keys(el).some(k => k.startsWith('__reactFiber') || k.startsWith('__reactContainer')))) frameworks.push('React');
  if (has('Vue') || sel('[data-v-app]') || sample.some(el => el.__vue__ || el.__vue_app__)) frameworks.push('Vue');
  if (sel('[ng-version]')) frameworks.push('Angular ' + sel('[ng-version]').getAttribute('ng-version'));
  if (sel('[class*="svelte-"]')) frameworks.push('Svelte');
  if (sel('astro-island')) frameworks.push('Astro');
  if (has('jQuery')) frameworks.push('jQuery ' + jQuery.fn.jquery);
  if (has('htmx')) frameworks.push('htmx');
  if (has('Alpine')) frameworks.push('Alpine.js');
  const generator = sel('meta[name="generator"]');
  return {
    frameworks, generator: generator ? generator.content : '',
    script_urls: scripts,
    inline_script_count: [...document.scripts].filter(s => !s.src).length,
    stylesheet_urls: uniq([...document.querySelectorAll('link[rel~="stylesheet"]')].map(l => l.href)),
    api_endpoints: uniq(resources.filter(r => r.initiator === 'xmlhttprequest' || r.initiator === 'fetch').map(r => r.url)),
    sourcemaps, resources,
  };
})()"#;

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
        .collect()
}

fn inspection_failed(err: BrowserError) -> CraftError {
    CraftError::InspectionFailed(err.to_string())
}

pub async fn inspect(
    request: &CraftRequest,
    store: &EvidenceStore,
    events: &EventSink,
    run_id: Uuid,
) -> Result<StudyReport, CraftError> {
    let url = match &request.location {
        TargetLocation::Url(u) => u.clone(),
        TargetLocation::Path(p) if p.is_file() => file_url(p),
        TargetLocation::Path(p) => {
            return Err(CraftError::InvalidTarget(format!(
                "{} is neither a URL nor an existing local page",
                p.display()
            )));
        }
    };
    let mut probe = Probe {
        store,
        events,
        run_id,
        source: "inspect::website",
        evidence_ids: Vec::new(),
    };
    let mut spec = CleanRoomSpec::new(
        request.target_kind,
        url.clone(),
        format!("Website at {url}. User goal: {}", request.prompt),
    );
    spec.requirements.push(
        BehavioralRequirement::new("REQ-WEB-GOAL", "goal", request.prompt.clone())
            .signal("The recreation fulfils the user's request."),
    );

    let mut browser = match Browser::launch().await {
        Ok(browser) => browser,
        Err(err) => {
            // Degrade instead of failing: the build can still proceed from the user's description.
            probe.warn(format!("Website capture skipped: {err}"));
            spec.open_questions.push(format!(
                "The page could not be captured ({err}); the layout and content are unknown."
            ));
            return Ok(StudyReport {
                clean_room_spec: spec,
                deep_findings: None,
                evidence_ids: probe.evidence_ids,
            });
        }
    };
    probe.info(format!("Headless browser: {}", browser.binary.display()));

    // Clean Room probes: what a visitor sees at desktop and phone sizes.
    let mut observed = Vec::new();
    for (name, viewport) in [("desktop", Viewport::DESKTOP), ("mobile", Viewport::MOBILE)] {
        browser.open(&url, viewport).await.map_err(inspection_failed)?;
        let png = browser.screenshot().await.map_err(inspection_failed)?;
        probe.save(
            &format!("screenshot-{name}"),
            EvidenceKind::Screenshot,
            &format!(
                "Original at {name} size ({}x{})",
                viewport.width, viewport.height
            ),
            &format!("original_{name}.png"),
            &png,
            true,
        )?;
        let page = browser.eval(OBSERVE_JS).await.map_err(inspection_failed)?;
        probe.save(
            &format!("visible-structure-{name}"),
            EvidenceKind::DomSnapshot,
            &format!("Visible structure and computed styles ({name})"),
            &format!("visible_structure_{name}.json"),
            &serde_json::to_vec_pretty(&page)?,
            true,
        )?;
        probe.info(format!(
            "Captured {name} view: {} headings, {} links, {} buttons",
            page["headings"].as_array().map_or(0, Vec::len),
            page["links"].as_array().map_or(0, Vec::len),
            page["buttons"].as_array().map_or(0, Vec::len),
        ));
        observed.push(page);
    }
    describe(&mut spec, &url, &request.prompt, &observed[0], &observed[1]);

    // Deep probes run only under an explicit, validated authorization.
    let deep_findings = if request.depth.is_deep() {
        probe.info("Deep mode authorized: inspecting network requests, bundles and frameworks");
        browser.open(&url, Viewport::DESKTOP).await.map_err(inspection_failed)?;
        let deep = browser.eval(DEEP_JS).await.map_err(inspection_failed)?;
        probe.save(
            "network-trace",
            EvidenceKind::NetworkTrace,
            "Network resources, bundles and framework fingerprints (deep)",
            "network_trace.json",
            &serde_json::to_vec_pretty(&deep)?,
            false,
        )?;
        Some(deep_findings(&deep))
    } else {
        None
    };

    Ok(StudyReport {
        clean_room_spec: spec,
        deep_findings,
        evidence_ids: probe.evidence_ids,
    })
}

/// Turns the observed page into the clean-room spec the builder works from.
fn describe(spec: &mut CleanRoomSpec, url: &str, prompt: &str, desktop: &Value, mobile: &Value) {
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    let title = text(&desktop["title"]);
    let description = text(&desktop["description"]);
    spec.overview = format!(
        "Website \"{title}\" at {url}.{} User goal: {prompt}",
        if description.is_empty() {
            String::new()
        } else {
            format!(" {description}")
        }
    );

    // Pages without a nav or header region still have links worth requiring.
    let nav_links = match strings(&desktop["nav_links"]) {
        links if links.is_empty() => strings(&desktop["links"]),
        links => links,
    };
    let buttons = strings(&desktop["buttons"]);
    let headings: Vec<(u64, String)> = desktop["headings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| (h["level"].as_u64().unwrap_or(2), text(&h["text"])))
        .collect();
    let field_labels: Vec<String> = desktop["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| format!("{} ({})", text(&f["label"]), text(&f["type"])))
        .collect();
    let image_alts: Vec<String> = desktop["images"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| text(&i["alt"]))
        .filter(|alt| !alt.is_empty())
        .collect();

    let mut features = vec![format!(
        "Page is {}px tall at {}px wide",
        desktop["page_height"], desktop["viewport"]["width"]
    )];
    for (label, items) in [
        ("Navigation links", &nav_links),
        ("Buttons", &buttons),
        ("Form fields", &field_labels),
        ("Images (alt text)", &image_alts),
    ] {
        if !items.is_empty() {
            features.push(format!("{label}: {}", items.join(", ")));
        }
    }
    let mobile_nav = strings(&mobile["nav_links"]).len();
    features.push(format!(
        "At phone width ({}px) the page is {}px tall and shows {mobile_nav} of {} navigation links",
        mobile["viewport"]["width"],
        mobile["page_height"],
        nav_links.len()
    ));
    let visible = text(&desktop["visible_text"]);
    if !visible.is_empty() {
        features.push(format!("Visible text: {visible}"));
    }
    spec.observed_features = features;

    spec.ui_structure = desktop["landmarks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| {
            let label = text(&l["label"]);
            format!(
                "{}{} at y={}px, {}px tall",
                text(&l["role"]),
                if label.is_empty() {
                    String::new()
                } else {
                    format!(" \"{label}\"")
                },
                l["top"],
                l["height"]
            )
        })
        .chain(headings.iter().map(|(level, heading)| {
            format!("{}h{level}: {heading}", "  ".repeat(*level as usize - 1))
        }))
        .collect();

    let palette = &desktop["palette"];
    let tokens = &mut spec.visual_tokens;
    tokens.insert("color.page_background".into(), text(&palette["page_background"]));
    for (group, key) in [("background", "backgrounds"), ("text", "text")] {
        for (i, color) in strings(&palette[key]).into_iter().enumerate() {
            tokens.insert(format!("color.{group}.{}", i + 1), color);
        }
    }
    for (i, font) in strings(&desktop["typography"]["fonts"]).into_iter().enumerate() {
        tokens.insert(format!("font.family.{}", i + 1), font);
    }
    for element in ["body", "h1", "h2", "link", "button"] {
        if let Some(style) = desktop["typography"][element].as_object() {
            for (property, value) in style {
                if let Some(value) = value.as_str() {
                    tokens.insert(format!("{element}.{property}"), value.to_string());
                }
            }
        }
    }

    // Checkable requirements: what the recreation must visibly contain.
    for (i, (_, heading)) in headings.iter().take(8).enumerate() {
        spec.requirements.push(
            BehavioralRequirement::new(
                format!("REQ-WEB-HEADING-{}", i + 1),
                "content",
                format!("The page shows the heading \"{heading}\""),
            )
            .expecting([heading.clone()]),
        );
    }
    for (id, area, what, items) in [
        ("REQ-WEB-NAV", "navigation", "Navigation offers these links", nav_links),
        ("REQ-WEB-BUTTONS", "interaction", "These buttons are present", buttons),
    ] {
        if !items.is_empty() {
            spec.requirements.push(
                BehavioralRequirement::new(id, area, format!("{what}: {}", items.join(", ")))
                    .expecting(items.into_iter().take(10)),
            );
        }
    }
}

fn deep_findings(deep: &Value) -> DeepFindings {
    let mut findings = DeepFindings::default();
    let frameworks = strings(&deep["frameworks"]);
    if !frameworks.is_empty() {
        findings
            .format_details
            .insert("frameworks".into(), frameworks.join(", "));
    }
    if let Some(generator) = deep["generator"].as_str().filter(|g| !g.is_empty()) {
        findings
            .format_details
            .insert("generator".into(), generator.to_string());
    }
    findings.format_details.insert(
        "inline_script_count".into(),
        deep["inline_script_count"].to_string(),
    );
    findings.extracted_symbols = strings(&deep["api_endpoints"]);
    let sourcemaps = strings(&deep["sourcemaps"]);
    findings.notes = vec![
        format!(
            "{} script bundle(s), {} stylesheet(s), {} resource request(s) observed",
            strings(&deep["script_urls"]).len(),
            strings(&deep["stylesheet_urls"]).len(),
            deep["resources"].as_array().map_or(0, Vec::len)
        ),
        format!(
            "{} XHR/fetch endpoint(s) called during load (listed in extracted_symbols)",
            findings.extracted_symbols.len()
        ),
        if sourcemaps.is_empty() {
            "No source maps exposed by same-origin scripts".into()
        } else {
            format!("{} exposed source map(s)", sourcemaps.len())
        },
    ];
    findings.assets_discovered = strings(&deep["script_urls"])
        .into_iter()
        .chain(strings(&deep["stylesheet_urls"]))
        .chain(sourcemaps.into_iter().map(|m| format!("sourcemap: {m}")))
        .collect();
    findings
}
