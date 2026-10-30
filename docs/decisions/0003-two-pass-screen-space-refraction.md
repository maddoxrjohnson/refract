# 3. Two-pass, screen-space glass

Status: accepted (M4)

**Context.** The gems are glass, and glass should bend what is behind it, including the other gems' glow. Ray tracing is out of reach on integrated graphics. A cube-map reflection would not show the scene behind the gem.

**Decision.** Each frame is drawn twice. First everything except the glass goes into a texture (the backdrop, the dust and each gem's lit core; the glass is on its own layer). Then the full scene is drawn through the post-processing chain, and each glass shell looks up that texture at its screen position, shifted by the view ray refracted through the facet, once per colour channel with slightly different indices. Samples per channel, texture size, bloom and depth of field vary by quality level.

**Consequences.** One extra scene render per frame, which is why quality levels exist. The glass does not refract other glass, because it is not in the texture, and the look is a screen-space approximation. In exchange it scales from integrated graphics to a desktop GPU with a single shader and no scene-specific tricks.
