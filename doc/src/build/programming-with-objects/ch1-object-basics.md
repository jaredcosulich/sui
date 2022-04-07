## Chapter 1: Object Basics

In Move, besides primitive data types, we can define organized data structures using `struct`. For example:
```
struct Color {
    red: u8,
    green: u8,
    blue: u8,
}
```
The above `struct` defines a data structure that can represents RGB color. Structures like this can be useful to organize data with complicated semantics. However, instances of structs like `Color` are not Move objects yet.
To define a struct that represents a Move object type, we must add a `key` capability to the definition, and the first field of the struct must be the `id` of the object with type `VersionedID` from the [ID library](../../../../sui_programmability/framework/sources/ID.move):
```
use Sui::ID::VersionedID;

struct ColorObject has key {
    id: VersionedID,
    red: u8,
    green: u8,
    blue: u8,
}
```
Now `ColorObject` represents a Sui object type and can be used to create Sui objects that can be eventually stored on the Sui chain.
