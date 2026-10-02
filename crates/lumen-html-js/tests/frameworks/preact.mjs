import { render } from 'preact';
import { useState } from 'preact/hooks';
import { jsx } from 'preact/jsx-runtime';
function App() {
  const [draft, setDraft] = useState('');
  const [todos, setTodos] = useState([]);
  return jsx('main', { children: [
    jsx('input', { id: 'draft', value: draft, onInput: e => setDraft(e.currentTarget.value) }, 'draft'),
    jsx('button', { id: 'add', onClick: () => { setTodos([...todos, draft]); setDraft(''); }, children: 'Add' }, 'add'),
    jsx('ul', { children: todos.map((text, i) => jsx('li', { children: text }, i)) }, 'todos')
  ] });
}
render(jsx(App, {}), document.getElementById('app'));
