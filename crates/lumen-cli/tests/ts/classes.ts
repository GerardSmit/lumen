// Classes: every erasable member form, and the JavaScript that must survive around them.
interface Shape {
  area(): number;
  readonly name: string;
}

abstract class Base<T extends object = {}> implements Shape {
  abstract area(): number;
  protected abstract readonly kind: string;
  declare meta: T;
  [key: string]: unknown;
  readonly name: string = "base";
  private secret?: number = 7;
  public static count: number = 0;
  #hidden: number = 1;

  constructor() {
    Base.count++;
  }

  describe(): string;
  describe(prefix: string): string;
  describe(prefix?: string): string {
    return `${prefix ?? ""}${this.name}:${this.kind}:${this.area().toFixed(2)}`;
  }

  protected get hidden(): number {
    return this.#hidden + (this.secret ?? 0);
  }

  public set hidden(v: number) {
    this.#hidden = v;
  }

  static {
    Base.count = 0;
  }
}

class Circle extends Base implements Shape {
  protected readonly kind = "circle";
  override readonly name: string = "circle";
  r: number;
  constructor(r: number = 1) {
    super();
    this.r = r;
  }
  override area(): number {
    return Math.PI * this.r ** 2;
  }
  peek(): number {
    return this.hidden;
  }
}

class Square extends Base {
  protected kind = "square";
  side!: number;
  constructor(side: number) {
    super();
    this.side = side;
  }
  area(): number {
    return this.side * this.side;
  }
}

const shapes: Shape[] = [new Circle(2), new Square(3)];
for (const s of shapes) console.log((s as Base).describe("> "));
console.log(Base.count, (shapes[0] as Circle).peek());
class Empty {}
class WithIn {
  x = 1
  public in() { return "in" }
  y = 2
  private ["computed"]() { return "c" }
}
const w = new WithIn() as any;
console.log(w.in(), w.computed(), Object.keys(new Empty()).length, w.x + w.y);
console.log(Circle.prototype.area.toString());
