// Bundles the extension entry point into a single CommonJS file for VS Code.
// `vscode` is provided by the host at runtime, so it is marked external.
// Also bundles the pi title extension (#1899), which pi — not VS Code — loads
// from dist/pi-title.mjs via `pi -e`.
const esbuild = require("esbuild");

const watch = process.argv.includes("--watch");

/** @type {import('esbuild').BuildOptions} */
const common = {
  bundle: true,
  platform: "node",
  target: "node18",
  format: "cjs",
  sourcemap: true,
  minify: !watch,
};

/** @type {import('esbuild').BuildOptions[]} */
const builds = [
  { ...common, entryPoints: ["src/extension.ts"], outfile: "dist/extension.js", external: ["vscode"] },
  // ESM: pi imports extensions through jiti, which does not unwrap a CJS
  // bundle's `exports.default`, so it would reject the factory.
  { ...common, entryPoints: ["src/piTitleExtension.ts"], outfile: "dist/pi-title.mjs", format: "esm" },
];

async function main() {
  if (watch) {
    for (const options of builds) {
      const ctx = await esbuild.context(options);
      await ctx.watch();
    }
    console.log("esbuild: watching src/extension.ts and src/piTitleExtension.ts …");
  } else {
    await Promise.all(builds.map((options) => esbuild.build(options)));
    console.log("esbuild: built dist/extension.js and dist/pi-title.mjs");
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
