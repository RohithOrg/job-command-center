// Shared data access and helpers for every resume template.
// All resume text arrives as JSON values, so it is only ever placed as plain content.

#let data = json("/data.json")
#let header = data.header
#let resume = data.resume

#let filled(s) = s != none and str(s).trim() != ""
#let any(xs) = xs != none and xs.len() > 0

// Lists with their blank entries dropped, so empty sections are omitted.
#let competencies = resume.competencies.filter(filled)
#let experience = resume.experience.filter(e => filled(e.company) or filled(e.role))
#let projects = resume.projects.filter(p => filled(p.name))
#let education = resume.education.filter(e => filled(e.title) or filled(e.org))
#let certifications = resume.certifications.filter(c => filled(c.title))
#let skills = resume.skills.filter(g => g.items.filter(filled).len() > 0)

// Non-empty strings joined by a separator.
#let joined(parts, sep) = parts.filter(filled).join(sep)

// Contact entries as content, clickable when they carry a url.
#let contact-items(style: it => it) = header.contacts.map(c => {
  if filled(c.url) { link(c.url, style(c.text)) } else { style(c.text) }
})

// Page, metadata and the text settings every template shares. Ligatures, hyphenation and
// justification stay off so an ATS reads whole, unbroken words.
#let setup(font: (), size: 10.5pt, margin: (x: 0.7in, y: 0.6in), leading: 0.6em, body) = {
  let title = if filled(header.name) { header.name + " - Resume" } else { "Resume" }
  set document(title: title, author: if filled(header.name) { header.name } else { () })
  set page(paper: data.paper, margin: margin)
  set text(
    font: font,
    size: size,
    lang: "en",
    ligatures: false,
    discretionary-ligatures: false,
    historical-ligatures: false,
    hyphenate: false,
  )
  set par(justify: false, leading: leading, spacing: leading)
  set list(marker: [•], indent: 0.2em, body-indent: 0.6em)
  body
}

// Calls `section(title, body)` for each non-empty section, in the standard order.
#let sections(
  section,
  summary: s => par(s),
  competencies-view: items => items.join(", "),
  job: none,
  project: none,
  degree: none,
  certification: none,
  skill-group: none,
) = {
  if filled(resume.summary) { section("Professional Summary", summary(resume.summary)) }
  if any(competencies) { section("Core Competencies", competencies-view(competencies)) }
  if any(experience) { section("Work Experience", experience.map(job).join()) }
  if any(projects) { section("Projects", projects.map(project).join()) }
  if any(education) { section("Education", education.map(degree).join()) }
  if any(certifications) { section("Certifications", certifications.map(certification).join()) }
  if any(skills) { section("Skills", skills.map(skill-group).join()) }
}

#let bullets(items) = {
  let items = items.filter(filled)
  if items.len() > 0 { list(..items) }
}
