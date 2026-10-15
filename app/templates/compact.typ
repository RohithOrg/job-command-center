// Compact: dense 10pt layout for long careers that must stay within two pages.
#import "common.typ": *

#let accent = rgb("#1d4e6e")
#let ink = rgb("#1b1e23")
#let muted = rgb("#5c636e")
#let hairline = rgb("#c9d1d9")

#show: setup.with(font: "Source Sans 3", size: 10pt, margin: (x: 0.55in, y: 0.5in), leading: 0.5em)
#set text(fill: ink)
#set list(marker: [•], body-indent: 0.5em, spacing: 0.42em)
#show link: set text(fill: ink)

#show heading.where(level: 1): it => block(above: 0.95em, below: 0.5em, sticky: true, {
  text(size: 10.5pt, weight: "bold", fill: accent, upper(it.body))
  h(6pt)
  box(width: 1fr, baseline: -0.32em, line(length: 100%, stroke: 0.5pt + hairline))
})

// Header: name and headline on one line, contacts below.
#block(below: 0.45em, {
  if filled(header.name) { text(size: 20pt, weight: "bold", header.name) }
  if filled(header.headline) { text(size: 11pt, fill: muted, [ #h(4pt) #text(fill: hairline)[|] #h(4pt) #header.headline]) }
})
#if header.contacts.len() > 0 {
  text(size: 9.25pt, fill: muted, contact-items().join([ #text(fill: hairline.darken(15%))[|] ]))
}

#let entry-head(left, right) = block(sticky: true, below: 0.4em, [#left #h(1fr) #right])

#sections(
  (title, body) => [= #title
    #body],
  competencies-view: items => items.join(", "),
  job: e => block(above: 0.75em, {
    entry-head(
      [#text(weight: "bold", e.role)#if filled(e.company) [, #text(weight: "semibold", fill: accent, e.company)]#if filled(e.location) [ #text(fill: muted)[| #e.location]]],
      text(size: 9.25pt, fill: muted, e.dates),
    )
    bullets(e.bullets)
  }),
  project: p => block(above: 0.55em, [
    #text(weight: "bold", p.name)#if filled(p.tech) [ #text(fill: muted, size: 9.25pt)[(#p.tech)]]#if filled(p.description) [: #p.description]
  ]),
  degree: e => block(above: 0.55em, {
    entry-head(
      [#text(weight: "bold", e.title)#if filled(e.org) [, #e.org]],
      text(size: 9.25pt, fill: muted, e.year),
    )
    if filled(e.description) { block(above: 0.3em, text(fill: muted, e.description)) }
  }),
  certification: c => block(above: 0.45em, entry-head(
    [#text(weight: "semibold", c.title)#if filled(c.org) [, #c.org]],
    text(size: 9.25pt, fill: muted, c.year),
  )),
  skill-group: g => block(above: 0.45em, [
    #if filled(g.category) [#text(weight: "bold", g.category + ":") ]
    #g.items.filter(filled).join(", ")
  ]),
)
