// Executive: serif type, centered name, restrained black and grey, generous spacing.
#import "common.typ": *

#let ink = rgb("#111111")
#let muted = rgb("#5a5a5a")
#let hairline = rgb("#bdbdbd")

#show: setup.with(font: "Libertinus Serif", size: 11pt, margin: (x: 0.85in, y: 0.7in), leading: 0.62em)
#set text(fill: ink)
#set list(marker: [–], body-indent: 0.6em, spacing: 0.6em)
#show link: set text(fill: ink)

#show heading.where(level: 1): it => block(
  width: 100%, above: 1.5em, below: 0.85em, inset: (bottom: 4pt), sticky: true,
  stroke: (bottom: 0.5pt + hairline),
  text(size: 13pt, weight: "regular", smallcaps(all: false, it.body)),
)

// Header
#align(center, {
  if filled(header.name) {
    block(below: 0.75em, text(size: 25pt, header.name))
  }
  if filled(header.headline) {
    block(below: 0.6em, text(size: 12pt, style: "italic", fill: muted, header.headline))
  }
  if header.contacts.len() > 0 {
    text(size: 10pt, fill: muted, contact-items().join([ #h(2pt)·#h(2pt) ]))
  }
})
#v(0.3em)

#let entry-head(left, right) = block(sticky: true, below: 0.5em, [#left #h(1fr) #right])

#sections(
  (title, body) => [= #title
    #body],
  competencies-view: items => items.join([ #h(1pt)·#h(1pt) ]),
  job: e => block(above: 1.15em, {
    entry-head(text(weight: "bold", e.company), text(fill: muted, e.dates))
    if filled(e.role) or filled(e.location) {
      entry-head(text(style: "italic", e.role), text(fill: muted, style: "italic", e.location))
    }
    bullets(e.bullets)
  }),
  project: p => block(above: 0.95em, {
    block(sticky: true, below: 0.45em, {
      text(weight: "bold", p.name)
      if filled(p.tech) { text(style: "italic", fill: muted, [ -- #p.tech]) }
    })
    if filled(p.description) { p.description }
  }),
  degree: e => block(above: 0.95em, {
    entry-head(text(weight: "bold", e.title), text(fill: muted, e.year))
    if filled(e.org) { block(above: 0.45em, text(style: "italic", e.org)) }
    if filled(e.description) { block(above: 0.45em, text(fill: muted, e.description)) }
  }),
  certification: c => block(above: 0.65em, entry-head(
    [#text(weight: "bold", c.title)#if filled(c.org) [, #text(style: "italic", c.org)]],
    text(fill: muted, c.year),
  )),
  skill-group: g => block(above: 0.65em, [
    #if filled(g.category) [#text(weight: "bold", g.category + ":") ]
    #g.items.filter(filled).join(", ")
  ]),
)
