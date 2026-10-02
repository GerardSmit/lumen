/** @jsxImportSource lumen */
const label = (value: number): string => `count ${value}`;
function view(value: number) {
    return <div title={label(value)}><span>{value}</span><img src="test.png" /></div>;
}
globalThis.tsxResult = view(4);
