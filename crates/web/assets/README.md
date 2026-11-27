# Vendored assets

`xterm.js`, `xterm.css` and `addon-fit.js` are the unmodified npm builds of
[`@xterm/xterm`](https://github.com/xtermjs/xterm.js) 5.5.0 (MIT) and
[`@xterm/addon-fit`](https://github.com/xtermjs/xterm.js/tree/master/addons/addon-fit)
0.10.0 (MIT). They are committed, not fetched at runtime: the UI has to work on
a machine without internet access, and a debugging tool should not hand its
keystrokes to a CDN.

To update:

```sh
curl -fsSL https://registry.npmjs.org/@xterm/xterm/-/xterm-5.5.0.tgz | tar xz
cp package/lib/xterm.js package/css/xterm.css .
curl -fsSL https://registry.npmjs.org/@xterm/addon-fit/-/addon-fit-0.10.0.tgz | tar xz
cp package/lib/addon-fit.js .
```

`index.html` is the only hand-written file in this directory; everything else
here is a build artifact of the two packages above or the logo.
