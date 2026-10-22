/-
# Encoding documents as leaf facts: empty containers (design.md §10.2)

§10.2 says each leaf of a document is one fact `(node, path, value)` and that maps (and
objects inside a `Document`) are structural: a key is a path segment. An *empty* map,
object, set, or struct with no set fields has no leaves, so it produces no facts. The
encoding therefore cannot distinguish `{}` from an absent field, or `{a: {}}` from `{}`.
-/
import ShepZSet.Facts

noncomputable section
open Classical

namespace Shep

/-- A document: a leaf value (scalars and atomic lists) or an object/map from keys to
sub-documents. -/
inductive Doc (Seg V : Type*) where
  | leaf : V → Doc Seg V
  | obj : List (Seg × Doc Seg V) → Doc Seg V

variable {N Seg V : Type*}

/-- The leaves of a document, with their paths. -/
def Doc.leaves : Doc Seg V → List (Path Seg × V)
  | .leaf v => [([], v)]
  | .obj [] => []
  | .obj ((k, d) :: rest) =>
      (d.leaves.map (fun qv => (k :: qv.1, qv.2))) ++ (Doc.obj rest).leaves

/-- The facts of a document stored at `(n, p)`: one `+1` per leaf. -/
def encodeDoc (n : N) (p : Path Seg) (d : Doc Seg V) : ZSet (Fact N Seg V) :=
  (d.leaves.map (fun qv => Finsupp.single (n, p ++ qv.1, qv.2) (1 : ℤ))).sum

/-- **The leaf-fact encoding is not injective on documents with empty containers**: an empty
object and an object holding an empty object encode to the same (empty) set of facts, and
both are indistinguishable from an absent field (no facts). -/
theorem encodeDoc_empty_containers (n : N) (p : Path Seg) (k : Seg) :
    (Doc.obj [] : Doc Seg V) ≠ Doc.obj [(k, Doc.obj [])] ∧
    encodeDoc n p (Doc.obj [] : Doc Seg V) = 0 ∧
    encodeDoc n p (Doc.obj [(k, Doc.obj [])] : Doc Seg V) = 0 := by
  refine ⟨?_, ?_, ?_⟩
  · intro h; cases h
  · simp [encodeDoc, Doc.leaves]
  · simp [encodeDoc, Doc.leaves]

end Shep
