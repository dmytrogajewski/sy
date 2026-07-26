# Preview Sample

This is the canonical preview sample for `sy-plugin-md` tests. The
fixture mirrors the shape of a typical project README so the golden
PNG is representative of what users will hover in the file manager.

## Section Two: Lists

The renderer must handle multiple paragraph types — these are the
common ones seen in real-world markdown:

- An unordered list item with a bit of length to it.
- A second item; the renderer should wrap long lines at the right
  margin without truncation.
- A third item with `inline code` embedded.

## Section Three: Code Block

Code blocks are rendered in a monospaced face on a darker background:

```rust
fn main() {
    println!("hello, sy file manager");
}
```

## Section Four: Inline Image

Below this line is an inline image reference. The renderer should
either composite it or fall back to a placeholder; the golden PNG
locks the resolved layout for the fixture.

![tiny test image](missing.png)

## Section Five: Link + Wrap

A [link to the sy repo](https://example.org/sy) — the renderer must
underline links with the accent colour. This last paragraph wraps
over multiple lines and exercises the right-margin reflow logic
under the renderer's default 800 px content width.

> A short blockquote at the end so the renderer's quote styling has
> a fixture to lock against.
