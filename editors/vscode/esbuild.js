// Bundles the extension entry point into a single CommonJS file for VS Code.
// `vscode` is provided by the host at runtime, so it is marked external.
const esbuild = require("esbuild");

const watch = process.argv.includes("--watch");

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints: ["src/extension.ts"],
  bundle: true,
  outfile: "dist/extension.js",
  platform: "node",
  target: "node18",
  format: "cjs",
  external: ["vscode"],
  sourcemap: true,
  minify: !watch,
};

async function main() {
  if (watch) {
    const ctx = await esbuild.context(options);
    await ctx.watch();
    console.log("esbuild: watching src/extension.ts …");
  } else {
    await esbuild.build(options);
    console.log("esbuild: built dist/extension.js");
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
