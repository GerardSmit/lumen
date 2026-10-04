// Regenerate with: node generate.cjs <directory containing Babel's node_modules>
const { createRequire } = require('node:module');
const { resolve } = require('node:path');
const { readFileSync, writeFileSync } = require('node:fs');
const { runInNewContext } = require('node:vm');
const dependency = createRequire(resolve(process.argv[2], 'package.json'));
const babel = dependency('@babel/core');
const jsx = dependency('@babel/plugin-transform-react-jsx');
const ts = dependency('@babel/plugin-transform-typescript');
const commonjs = dependency('@babel/plugin-transform-modules-commonjs');
const typescriptCompiler = dependency('typescript');
const runtime = {
    Fragment: 'fragment',
    jsx(type, props, key) { return { type, props, key }; },
    createElement(type, props, ...children) { return { type, props, children, mode: 'classic' }; },
};
runtime.jsxs = runtime.jsx;
const classic = 'const React={Fragment:"fragment",createElement(type,props,...children){return {type,props,children,mode:"classic"};}};';
const cases = [
    ['automatic-basic', false, 'automatic', 'globalThis.result=<div id="x" flag>{1+2}</div>;'],
    ['automatic-fragment', false, 'automatic', 'globalThis.result=<><a/>hello<b/></>;'],
    ['automatic-key-before-spread', false, 'automatic', 'const p={key:"spread",a:1};globalThis.result=<a key="early" {...p}/>;'],
    ['automatic-key-after-spread', false, 'automatic', 'const p={key:"spread",a:1};globalThis.result=<a {...p} key="late"/>;'],
    ['entities', false, 'automatic', 'globalThis.result=<a title="&copy;&Omega;&#x1f642;">&amp;&lt;&#65;&unknown;</a>;'],
    ['whitespace', false, 'automatic', 'globalThis.result=<a>\n  hello\n  world\n <b/> </a>;'],
    ['attribute-newlines', false, 'automatic', 'globalThis.result=<a title="hello\n  world"/>;'],
    ['namespaced', false, 'automatic', 'globalThis.result=<svg:path xml:lang="en"/>;'],
    ['nested-expressions', false, 'automatic', 'const n=3;globalThis.result=<a>{n>2?<b/>:null}{(()=> <c>{n}</c>)()}</a>;'],
    ['comment-and-string', false, 'automatic', 'const text="<tag/>";globalThis.result=<a>{/* empty */}{text}</a>;'],
    ['classic-member', false, 'classic', classic + 'const UI={Button:"button"};globalThis.result=<UI.Button {...{n:3}}>a{4}</UI.Button>;'],
    ['classic-fragment', false, 'classic', classic + 'globalThis.result=<><a/>hello</>;'],
    ['tsx-generics', true, 'automatic', 'const id=<T,>(x:T):T=>x;const n:number=3;globalThis.result=<a n={id<number>(n)}>{n+1}</a>;'],
    ['tsx-constraint', true, 'automatic', 'const id=<T extends number>(x:T):T=>x;globalThis.result=<a>{id(4)}</a>;'],
    ['tsx-default', true, 'automatic', 'const id=<T=number>(x:T):T=>x;globalThis.result=<a>{id(5)}</a>;'],
];
const fixtures = cases.map(([name, typescript, runtimeMode, source]) => {
    const plugins = typescript ? [[ts, { isTSX: true, allExtensions: true }]] : [];
    plugins.push([jsx, { runtime: runtimeMode, ...(runtimeMode === 'automatic' ? { importSource: 'fixture' } : {}), throwIfNamespace: false }], commonjs);
    const code = babel.transformSync(source, { filename: `${name}.tsx`, configFile: false, babelrc: false, plugins }).code;
    const context = { require() { return runtime; }, exports: {} };
    runInNewContext(code, context);
    if (typescript) {
        const result = typescriptCompiler.transpileModule(source, { fileName: `${name}.tsx`, reportDiagnostics: true, compilerOptions: { module: typescriptCompiler.ModuleKind.CommonJS, target: typescriptCompiler.ScriptTarget.ES2022, jsx: typescriptCompiler.JsxEmit.ReactJSX, jsxImportSource: 'fixture' } });
        if (result.diagnostics.length) throw new Error(JSON.stringify(result.diagnostics));
        const tsContext = { require() { return runtime; }, exports: {} };
        runInNewContext(result.outputText, tsContext);
        if (JSON.stringify(tsContext.result) !== JSON.stringify(context.result)) throw new Error(`Babel/TypeScript mismatch: ${name}`);
    }
    return { name, typescript, runtime: runtimeMode, source, expected: JSON.stringify(context.result) };
});
const output = JSON.stringify({
    generator: 'Babel plugin-transform-react-jsx (MIT)',
    typescript: typescriptCompiler.version,
    versions: Object.fromEntries(['@babel/core', '@babel/plugin-transform-react-jsx', '@babel/plugin-transform-typescript', '@babel/plugin-transform-modules-commonjs'].map(name => [name, dependency(`${name}/package.json`).version])),
    fixtures,
}, null, 2) + '\n';
const target = resolve(__dirname, 'babel.json');
if (process.argv.includes('--verify')) {
    if (readFileSync(target, 'utf8') !== output) throw new Error('babel.json is out of date');
} else {
    writeFileSync(target, output);
}
