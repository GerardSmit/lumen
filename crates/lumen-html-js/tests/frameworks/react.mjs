import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { jsx } from 'react/jsx-runtime';
function App() {
  const [draft, setDraft] = useState('');
  const [todos, setTodos] = useState([]);
  return jsx('main', { children: [
    jsx('input', { id: 'draft', value: draft, onChange: e => setDraft(e.currentTarget.value) }, 'draft'),
    jsx('button', { id: 'add', onClick: () => { setTodos([...todos, draft]); setDraft(''); }, children: 'Add' }, 'add'),
    jsx('ul', { children: todos.map((text, i) => jsx('li', { children: text }, i)) }, 'todos')
  ] });
}
createRoot(document.getElementById('app')).render(jsx(App, {}));
