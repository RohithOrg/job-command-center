// Modern: bold name, small-caps titles with an accent bar, competencies as subtle chips.
#import "common.typ": *

#let accent = rgb("#3b3fb6")
#let ink = rgb("#16181d")
#let muted = rgb("#5f6573")
#let chip = rgb("#eef0fb")

#show: setup.with(font: "IBM Plex Sans", size: 10pt, margin: (x: 0.72in, y: 0.62in), leading: 0.62em)
#set text(fill: ink)
#set list(marker: text(fill: accent)[•], spacing: 0.55em)
#show link: set text(fill: ink)

// Plex has no small-caps glyphs, so titles are set as small capitals. No letter tracking:
// spaced-out letters extract as "S U M M A R Y".
#show heading.where(level: 1): it => block(
  above: 1.35em, below: 0.75em, sticky: true,
  stroke: (left: 2.5pt + accent), inset: (left: 7pt, y: 1.5pt),
  text(size: 9.5pt, weight: "bold", upper(it.body)),
)

// Header
#if filled(header.name) {
  block(below: 0.7em, text(size: 28pt, weight: "bold", header.name))
}
#if filled(header.headline) {
  block(below: 0.6em, text(size: 11.5pt, weight: "semibold", fill: accent, header.headline))
}
#if header.contacts.len() > 0 {
  block(below: 0.2em, text(size: 9pt, fill: muted, contact-items().join([ #text(fill: accent)[·] ])))
}

#let entry-head(left, right) = block(sticky: true, below: 0.5em, [#left #h(1fr) #right])

#sections(
  (title, body) => [= #title
    #body],
  competencies-view: items => {
    set par(leading: 0.85em)
    items.map(c => box(fill: chip, radius: 3pt, inset: (x: 6pt, y: 3pt), text(size: 9pt, fill: accent.darken(15%), c))).join(" ")
  },
  job: e => block(above: 1.05em, {
    entry-head(text(size: 10.5pt, weight: "bold", e.role), text(size: 9pt, weight: "semibold", fill: accent, e.dates))
    if filled(e.company) or filled(e.location) {
      entry-head(text(weight: "semibold", fill: muted, e.company), text(size: 9pt, fill: muted, e.location))
    }
    bullets(e.bullets)
  }),
  project: p => block(above: 0.9em, {
    block(sticky: true, below: 0.45em, {
      text(weight: "bold", p.name)
      if filled(p.tech) { text(size: 9pt, fill: accent, [ · #p.tech]) }
    })
    if filled(p.description) { p.description }
  }),
  degree: e => block(above: 0.9em, {
    entry-head(text(weight: "bold", e.title), text(size: 9pt, weight: "semibold", fill: accent, e.year))
    if filled(e.org) { block(above: 0.4em, text(fill: muted, weight: "semibold", e.org)) }
    if filled(e.description) { block(above: 0.4em, text(fill: muted, e.description)) }
  }),
  certification: c => block(above: 0.6em, entry-head(
    [#text(weight: "semibold", c.title) #if filled(c.org) [#text(fill: muted)[· #c.org]]],
    text(size: 9pt, weight: "semibold", fill: accent, c.year),
  )),
  skill-group: g => block(above: 0.6em, [
    #if filled(g.category) [#text(weight: "bold", g.category + ":") ]
    #g.items.filter(filled).join(", ")
  ]),
)
