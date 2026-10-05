/** @jsxImportSource lumen */
const { signal, For } = __lumen;
const MAX_TODOS = 128;
let nextId = 1;
const todoIds = signal([]);
const draft = signal("");
const filter = signal("all");
const reversed = signal(false);
const editing = signal(null);
const editDraft = signal("");
const tasks = new Map();

function appendTodo(current, title) {
  const text = String(title).trim();
  if (!text || current.length >= MAX_TODOS) return [current, false];
  const id = nextId++;
  tasks.set(id, { id, title: signal(text), completed: signal(false) });
  return [[...current, id], true];
}

function addTodo(value) {
  const fromDraft = value === undefined;
  const [next, added] = appendTodo(todoIds[0](), fromDraft ? draft[0]() : value);
  if (!added) return false;
  todoIds[1](next);
  if (fromDraft) draft[1]("");
  return true;
}

function visibleIds() {
  const mode = filter[0]();
  const ids = todoIds[0]().filter((id) => {
    const completed = tasks.get(id).completed[0]();
    return mode === "all" || (mode === "completed" ? completed : !completed);
  });
  return reversed[0]() ? ids.reverse() : ids;
}

function activeCount() {
  let count = 0;
  for (const id of todoIds[0]()) if (!tasks.get(id).completed[0]()) count++;
  return count;
}

function removeTodo(id) {
  tasks.delete(id);
  todoIds[1](todoIds[0]().filter((item) => item !== id));
  if (editing[0]() === id) editing[1](null);
}

function beginEdit(id) {
  editing[1](id);
  editDraft[1](tasks.get(id).title[0]());
}

function commitEdit(id) {
  const title = editDraft[0]().trim();
  if (!title) removeTodo(id);
  else tasks.get(id).title[1](title);
  editing[1](null);
}

function onEditKey(event, id) {
  globalThis.profileLastEditKey = event.key;
  if (event.key === "Enter") {
    event.preventDefault();
    commitEdit(id);
  } else if (event.key === "Escape") {
    event.preventDefault();
    editing[1](null);
  }
}

function renderTodo(id) {
  const todo = tasks.get(id);
  const row = <li id={"todo-" + id} data-id={id} className={"todo" + (todo.completed[0]() ? " completed" : "") + (editing[0]() === id ? " editing" : "")}>
    <input id={"toggle-" + id} type="checkbox" className="toggle" checked={todo.completed[0]()} />
    <label id={"label-" + id} className={"todo-label" + (editing[0]() === id ? " hidden" : "")}>{todo.title[0]()}</label>
    <button id={"edit-" + id} type="button">Edit</button>
    <button id={"delete-" + id} type="button">Delete</button>
    <input id={"editor-" + id} type="text" className={"editor" + (editing[0]() === id ? "" : " hidden")} value={editing[0]() === id ? editDraft[0]() : todo.title[0]()} />
  </li>;
  row.querySelector(".toggle").addEventListener("change", (event) => {
    todo.completed[1](event.currentTarget.checked);
  });
  row.querySelector("#edit-" + id).addEventListener("click", () => beginEdit(id));
  row.querySelector("#delete-" + id).addEventListener("click", () => removeTodo(id));
  const editor = row.querySelector("#editor-" + id);
  editor.addEventListener("input", (event) => editDraft[1](event.currentTarget.value));
  editor.addEventListener("keydown", (event) => onEditKey(event, id));
  return row;
}

const list = For({ each: visibleIds, children: (id) => renderTodo(id) });
const app = <main id="todos" className="todoapp">
  <h1>todos</h1>
  <div id="entry">
    <input id="new-todo" type="text" placeholder="What needs to be done?" value={draft[0]()} />
    <button id="add" type="button">Add</button>
    <button id="reverse" type="button">Reverse</button>
  </div>
  <ul id="todo-list">{list}</ul>
  <nav id="filters">
    <button id="filter-all" type="button" aria-pressed={filter[0]() === "all"}>All</button>
    <button id="filter-active" type="button" aria-pressed={filter[0]() === "active"}>Active</button>
    <button id="filter-completed" type="button" aria-pressed={filter[0]() === "completed"}>Completed</button>
  </nav>
  <p id="count">{activeCount()} items left</p>
</main>;
document.getElementById("app").appendChild(app);

document.getElementById("new-todo").addEventListener("input", (event) => draft[1](event.currentTarget.value));
document.getElementById("new-todo").addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    addTodo();
  }
});
document.getElementById("add").addEventListener("click", () => addTodo());
document.getElementById("reverse").addEventListener("click", () => reversed[1](!reversed[0]()));
document.getElementById("filter-all").addEventListener("click", () => filter[1]("all"));
document.getElementById("filter-active").addEventListener("click", () => filter[1]("active"));
document.getElementById("filter-completed").addEventListener("click", () => filter[1]("completed"));

globalThis.profileAddTodo = (title) => addTodo(title);
globalThis.profileFillToLimit = () => {
  let current = todoIds[0]();
  while (current.length < MAX_TODOS) {
    const [next, added] = appendTodo(current, "profile " + (current.length + 1));
    if (!added) break;
    current = next;
  }
  todoIds[1](current);
  return current.length;
};
globalThis.profileCount = () => todoIds[0]().length;
