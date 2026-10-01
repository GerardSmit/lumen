class Room:
    def __init__(self, name, desc, items=None):
        self.name = name
        self.desc = desc
        self.items = list(items or [])
        self.exits = {}
        self.locked = {}


class Game:
    OPPOSITE = {"north": "south", "south": "north", "east": "west", "west": "east", "up": "down", "down": "up"}

    def __init__(self):
        self.rooms = {}
        self.state = "playing"
        self.inventory = []
        self.moves = 0
        self.score = 0
        self.here = None
        self.visited = set()
        self.build()

    def connect(self, a, direction, b, key=None):
        self.rooms[a].exits[direction] = b
        self.rooms[b].exits[self.OPPOSITE[direction]] = a
        if key:
            self.rooms[a].locked[direction] = key
            self.rooms[b].locked[self.OPPOSITE[direction]] = key

    def build(self):
        r = self.rooms
        r["hall"] = Room("Hall", "A dusty entrance hall.", ["lamp"])
        r["kitchen"] = Room("Kitchen", "Pots hang from the ceiling.", ["knife", "bread"])
        r["cellar"] = Room("Cellar", "Dark and damp.", ["key"])
        r["library"] = Room("Library", "Shelves of old books.", ["book"])
        r["tower"] = Room("Tower", "Wind howls through the windows.", ["gem"])
        r["vault"] = Room("Vault", "Gold glitters everywhere.", ["crown"])
        self.connect("hall", "east", "kitchen")
        self.connect("kitchen", "down", "cellar")
        self.connect("hall", "north", "library")
        self.connect("library", "up", "tower")
        self.connect("hall", "west", "vault", key="key")
        self.here = "hall"
        self.visited.add("hall")

    def dark(self):
        return self.here == "cellar" and "lamp" not in self.inventory

    def look(self):
        room = self.rooms[self.here]
        if self.dark():
            return "It is pitch black."
        parts = [f"[{room.name}] {room.desc}"]
        if room.items:
            parts.append("You see: " + ", ".join(sorted(room.items)) + ".")
        parts.append("Exits: " + ", ".join(sorted(room.exits)) + ".")
        return " ".join(parts)

    def do(self, line):
        words = line.lower().split()
        if not words:
            return "Say something."
        verb, args = words[0], words[1:]
        handler = getattr(self, "cmd_" + verb, None)
        if handler is None:
            return f"I don't know how to '{verb}'."
        self.moves += 1
        return handler(*args)

    def cmd_look(self):
        return self.look()

    def cmd_go(self, direction=None):
        room = self.rooms[self.here]
        if direction not in room.exits:
            return "You can't go that way."
        need = room.locked.get(direction)
        if need and need not in self.inventory:
            return f"The way {direction} is locked."
        self.here = room.exits[direction]
        if self.here not in self.visited:
            self.visited.add(self.here)
            self.score += 5
        if self.here == "vault" and "crown" not in self.rooms["vault"].items:
            pass
        return self.look()

    def cmd_take(self, item=None):
        if self.dark():
            return "You fumble in the dark."
        room = self.rooms[self.here]
        if item == "all":
            taken = sorted(room.items)
            self.inventory.extend(taken)
            self.score += 10 * len(taken)
            room.items.clear()
            return "Taken: " + (", ".join(taken) or "nothing")
        if item not in room.items:
            return f"No {item} here."
        room.items.remove(item)
        self.inventory.append(item)
        self.score += 10
        if item == "crown":
            self.state = "won"
            return "You take the crown. You win!"
        return f"Taken: {item}."

    def cmd_drop(self, item=None):
        if item not in self.inventory:
            return f"You don't have {item}."
        self.inventory.remove(item)
        self.rooms[self.here].items.append(item)
        return f"Dropped: {item}."

    def cmd_inventory(self):
        return "Carrying: " + (", ".join(self.inventory) if self.inventory else "nothing")

    def cmd_eat(self, item=None):
        if item != "bread" or "bread" not in self.inventory:
            return "You can't eat that."
        self.inventory.remove("bread")
        self.score += 2
        return "Yum."

    def cmd_quit(self):
        self.state = "quit"
        return "Bye."


script = """
look
go north
take book
take sword
go up
take gem
go down
go south
go west
go east
take all
eat bread
dance
go down
look
take key
go up
go up
go west
look
drop gem
inventory
go west
take crown
look
""".strip().splitlines()

game = Game()
print(game.look())
for n, line in enumerate(script, 1):
    if game.state != "playing":
        print("game over:", game.state)
        break
    print(f"{n:2}> {line}")
    print("   ", game.do(line))
print("state:", game.state)
print("score:", game.score, "moves:", game.moves)
print("inventory:", sorted(game.inventory))
print("visited:", sorted(game.visited))
print({k: sorted(v.items) for k, v in sorted(game.rooms.items())})
print(game.do(""))
print(game.do("quit"), game.state)
