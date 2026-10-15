// Classic: large name, teal-to-purple rule, teal section titles, purple companies.
#import "common.typ": *

#let teal = color.hsl(187deg, 74%, 32%)
#let purple = color.hsl(270deg, 70%, 45%)
#let ink = rgb("#1f2328")
#let muted = rgb("#5b6270")
#let hairline = rgb("#d7dce2")

#show: setup.with(font: "Source Sans 3", size: 10.5pt, margin: (x: 0.7in, y: 0.6in))
#set text(fill: ink)
#show link: set text(fill: ink)

#show heading.where(level: 1): it => block(
  width: 100%, above: 1.2em, below: 0.65em, inset: (bottom: 3.5pt), sticky: true,
  stroke: (bottom: 0.6pt + hairline),
  text(size: 11.5pt, weight: "semibold", fill: teal, upper(it.body)),
)

// Header
#if filled(header.name) {
  block(below: 0pt, text(size: 26pt, weight: "bold", header.name))
}
#block(above: 0.55em, below: 0.6em, rect(
  width: 100%, height: 2pt, stroke: none,
  fill: gradient.linear(teal, purple),
))
#if filled(header.headline) {
  block(below: 0.35em, text(size: 12pt, weight: "semibold", fill: teal, header.headline))
}
#if header.contacts.len() > 0 {
  text(size: 9.5pt, fill: muted, contact-items().join([ #text(fill: hairline.darken(20%))[|] ]))
}

#let entry-head(left, right) = block(sticky: true, below: 0.45em, [#left #h(1fr) #right])

#sections(
  (title, body) => [= #title
    #body],
  competencies-view: items => text(size: 10pt, items.join([ #text(fill: teal)[•] ])),
  job: e => block(above: 1em, {
    entry-head(
      text(weight: "semibold", fill: purple, size: 11pt, e.company),
      text(fill: muted, size: 9.5pt, e.dates),
    )
    if filled(e.role) or filled(e.location) {
      entry-head(text(weight: "semibold", e.role), text(fill: muted, size: 9.5pt, style: "italic", e.location))
    }
    bullets(e.bullets)
  }),
  project: p => block(above: 0.85em, {
    block(sticky: true, below: 0.4em, {
      text(weight: "semibold", fill: purple, p.name)
      if filled(p.tech) { text(fill: muted, size: 9.5pt, [ | #p.tech]) }
    })
    if filled(p.description) { p.description }
  }),
  degree: e => block(above: 0.8em, {
    entry-head(text(weight: "semibold", e.title), text(fill: muted, size: 9.5pt, e.year))
    if filled(e.org) { block(above: 0.35em, text(fill: purple, e.org)) }
    if filled(e.description) { block(above: 0.35em, text(fill: muted, e.description)) }
  }),
  certification: c => block(above: 0.55em, entry-head(
    [#text(weight: "semibold", c.title) #if filled(c.org) [#text(fill: muted)[|] #c.org]],
    text(fill: muted, size: 9.5pt, c.year),
  )),
  skill-group: g => block(above: 0.55em, [
    #if filled(g.category) [#text(weight: "semibold", fill: teal, g.category + ":") ]
    #g.items.filter(filled).join(", ")
  ]),
)
