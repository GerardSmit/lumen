import { render } from "../frameworks/node_modules/preact/dist/preact.mjs";
import { useState } from "../frameworks/node_modules/preact/hooks/dist/hooks.mjs";
import { jsx, jsxs } from "../frameworks/node_modules/preact/jsx-runtime/dist/jsxRuntime.mjs";

const MAX_TODOS = 128;
let nextId = 1;

function appendTodo(current, title) {
  const text = String(title).trim();
  if (!text || current.length >= MAX_TODOS) return [current, false];
  const todo = { id: nextId++, text, completed: false };
  return [[...current, todo], true];
}

function TodoRow({ todo, editing, editDraft, onToggle, onEdit, onDelete, onEditInput, onEditKey }) {
  const isEditing = editing === todo.id;
  return jsxs("li", {
    id: "todo-" + todo.id,
    "data-id": String(todo.id),
    className: "todo" + (todo.completed ? " completed" : "") + (isEditing ? " editing" : ""),
    children: [
      jsx("input", {
        id: "toggle-" + todo.id,
        type: "checkbox",
        className: "toggle",
        checked: todo.completed,
        onChange: (event) => onToggle(todo.id, event.currentTarget.checked),
      }),
      jsx("label", {
        id: "label-" + todo.id,
        className: "todo-label" + (isEditing ? " hidden" : ""),
        children: todo.text,
      }),
      jsx("button", { id: "edit-" + todo.id, type: "button", onClick: () => onEdit(todo.id), children: "Edit" }),
      jsx("button", { id: "delete-" + todo.id, type: "button", onClick: () => onDelete(todo.id), children: "Delete" }),
      jsx("input", {
        id: "editor-" + todo.id,
        type: "text",
        className: "editor" + (isEditing ? "" : " hidden"),
        value: isEditing ? editDraft : todo.text,
        onInput: (event) => onEditInput(event.currentTarget.value),
        onKeyDown: (event) => onEditKey(event, todo.id),
      }),
    ],
  });
}

function App() {
  const [todos, setTodos] = useState([]);
  const [draft, setDraft] = useState("");
  const [filter, setFilter] = useState("all");
  const [reversed, setReversed] = useState(false);
  const [editing, setEditing] = useState(null);
  const [editDraft, setEditDraft] = useState("");

  function addTodo(value) {
    const fromDraft = value === undefined;
    const [next, added] = appendTodo(todos, fromDraft ? draft : value);
    if (!added) return false;
    setTodos(next);
    if (fromDraft) setDraft("");
    return true;
  }

  function toggleTodo(id, completed) {
    setTodos(todos.map((todo) => todo.id === id ? { ...todo, completed } : todo));
  }

  function removeTodo(id) {
    setTodos(todos.filter((todo) => todo.id !== id));
    if (editing === id) setEditing(null);
  }

  function beginEdit(id) {
    const todo = todos.find((item) => item.id === id);
    setEditing(id);
    setEditDraft(todo.text);
  }

  function commitEdit(id) {
    const text = editDraft.trim();
    if (!text) removeTodo(id);
    else setTodos(todos.map((todo) => todo.id === id ? { ...todo, text } : todo));
    setEditing(null);
  }

  function onEditKey(event, id) {
    globalThis.profileLastEditKey = event.key;
    if (event.key === "Enter") {
      event.preventDefault();
      commitEdit(id);
    } else if (event.key === "Escape") {
      event.preventDefault();
      setEditing(null);
    }
  }

  let visible = todos.filter((todo) =>
    filter === "all" || (filter === "completed" ? todo.completed : !todo.completed)
  );
  if (reversed) visible = visible.slice().reverse();
  const activeCount = todos.filter((todo) => !todo.completed).length;

  globalThis.profileAddTodo = (title) => addTodo(title);
  globalThis.profileFillToLimit = () => {
    let current = todos;
    while (current.length < MAX_TODOS) {
      const [next, added] = appendTodo(current, "profile " + (current.length + 1));
      if (!added) break;
      current = next;
    }
    setTodos(current);
    return current.length;
  };
  globalThis.profileCount = () => todos.length;

  return jsxs("main", {
    id: "todos",
    className: "todoapp",
    children: [
      jsx("h1", { children: "todos" }),
      jsxs("div", {
        id: "entry",
        children: [
          jsx("input", {
            id: "new-todo",
            type: "text",
            placeholder: "What needs to be done?",
            value: draft,
            onInput: (event) => setDraft(event.currentTarget.value),
            onKeyDown: (event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                addTodo();
              }
            },
          }),
          jsx("button", { id: "add", type: "button", onClick: () => addTodo(), children: "Add" }),
          jsx("button", { id: "reverse", type: "button", onClick: () => setReversed(!reversed), children: "Reverse" }),
        ],
      }),
      jsx("ul", {
        id: "todo-list",
        children: visible.map((todo) => jsx(TodoRow, {
          todo, editing, editDraft, onToggle: toggleTodo, onEdit: beginEdit,
          onDelete: removeTodo, onEditInput: setEditDraft, onEditKey,
        }, todo.id)),
      }),
      jsxs("nav", {
        id: "filters",
        children: [
          jsx("button", { id: "filter-all", type: "button", "aria-pressed": filter === "all" ? "true" : "false", onClick: () => setFilter("all"), children: "All" }),
          jsx("button", { id: "filter-active", type: "button", "aria-pressed": filter === "active" ? "true" : "false", onClick: () => setFilter("active"), children: "Active" }),
          jsx("button", { id: "filter-completed", type: "button", "aria-pressed": filter === "completed" ? "true" : "false", onClick: () => setFilter("completed"), children: "Completed" }),
        ],
      }),
      jsx("p", { id: "count", children: activeCount + " items left" }),
    ],
  });
}

render(jsx(App, {}), document.getElementById("app"));
