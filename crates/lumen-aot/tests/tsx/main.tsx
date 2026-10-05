/** @jsxImportSource ./runtime */
const id = <T,>(value: T): T => value;
function view(value: number) {
    return <section count={id(value)}>{value + 1}</section>;
}
globalThis.tsxResult = JSON.stringify(view(4));
