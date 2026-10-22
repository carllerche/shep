/-
# Combining policies: union minus deny, field policies, and the Ent port (design.md §6.1, §6.6)

Restatement-round obligations 5 and 10 (§18). Guards are two-valued (§4.0 item 1, proven in
`Totality.lean`), so a policy's decision is a formula over the guards' truth values. Guards are
abstract here: a list of guards of any type `G`, and an evaluation `ev : G → Prop` at one
request and state.

## Union minus deny (`umd`)

`umd ev A D := (∨ A) ∧ ¬(∨ D)`.

* `umd_default_deny`: with no allow that holds, the answer is deny.
* `umd_deny_wins`: a deny that holds beats every allow.
* `umd_perm`: order doesn't matter.
* `umd_mono_allow`, `umd_antimono_deny`: adding an allow only widens, adding a deny only narrows.

## Field policies (`fieldOK`)

§6.1: `type_allowed ∧ (no field allows ∨ ∨A_f) ∧ ¬∨D_f`.

* `field_default_deny`: **default deny for fields with allows**: if the field has allows and
  none holds, the field operation is denied, whatever the type says.
* `field_le_type`: **a field policy never grants a field operation the type denies.**
* `field_type_default_deny`: with no type allow holding, every field operation is denied.
* `field_denies_only`, `field_with_allows`: the two cases of the rule.
* `field_not_replace` (**doc finding**): §6.1's gloss "a field with allows *replaces* the type's
  allows for that field" is not what the formula says. The formula keeps `type_allowed`, so a
  field allow can't grant a request the type's allows don't; a replacing rule would.
* `first_field_allow_narrows` (**doc finding**): adding the *first* allow to a field can make
  the field strictly more restrictive (it switches the field from "type decides" to "one of
  these must hold"). Every other allow addition widens (`field_mono_allow`).

## The Ent port (`firstMatch`, `port`)

Ordered first-match rules with default deny (`firstMatch`), and the rewrite of §6.1:
keep every allow, and replace deny `D_j` with `D'_j = D_j ∧ ¬(∨_{i<j} A_i)`.

* `port_exact`: **first match equals union minus deny after the rewrite**, for every rule list.
* `port_denies_first`: a list whose denies all come first needs no rewrite.
* `port_entDefault`: Ent itself treats "every rule skips" as allow; porting such a policy also
  needs a final `allow true` (exact with it, `port_entDefault`).
-/
import ShepZSet.Basic

namespace Shep
namespace PolicyCombine

variable {G : Type*}

/-! ## Union minus deny -/

/-- **Union minus deny**: some allow holds and no deny holds. -/
def umd (ev : G → Prop) (A D : List G) : Prop := (∃ a ∈ A, ev a) ∧ ¬ ∃ d ∈ D, ev d

/-- **Default deny.** -/
theorem umd_default_deny {ev : G → Prop} {A : List G} (D : List G) (h : ∀ a ∈ A, ¬ ev a) :
    ¬ umd ev A D := fun ⟨⟨a, ha, hev⟩, _⟩ => h a ha hev

/-- With no allows at all, everything is denied. -/
theorem umd_nil (ev : G → Prop) (D : List G) : ¬ umd ev [] D :=
  umd_default_deny D (by simp)

/-- **A deny that holds beats every allow.** -/
theorem umd_deny_wins {ev : G → Prop} (A : List G) {D : List G} {d : G} (hd : d ∈ D)
    (h : ev d) : ¬ umd ev A D := fun ⟨_, hn⟩ => hn ⟨d, hd, h⟩

/-- **Order doesn't matter.** -/
theorem umd_perm {ev : G → Prop} {A A' D D' : List G} (hA : A.Perm A') (hD : D.Perm D') :
    umd ev A D ↔ umd ev A' D' := by
  simp only [umd, hA.mem_iff, hD.mem_iff]

/-- Adding an allow only widens. -/
theorem umd_mono_allow {ev : G → Prop} {A D : List G} (a : G) (h : umd ev A D) :
    umd ev (a :: A) D :=
  ⟨by obtain ⟨⟨b, hb, hev⟩, _⟩ := h; exact ⟨b, List.mem_cons_of_mem _ hb, hev⟩, h.2⟩

/-- Adding a deny only narrows. -/
theorem umd_antimono_deny {ev : G → Prop} {A D : List G} (d : G) (h : umd ev A (d :: D)) :
    umd ev A D :=
  ⟨h.1, fun ⟨e, he, hev⟩ => h.2 ⟨e, List.mem_cons_of_mem _ he, hev⟩⟩

/-! ## Field policies -/

/-- §6.1: `type_allowed ∧ (no field allows ∨ ∨A_f) ∧ ¬∨D_f`. -/
def fieldOK (ev : G → Prop) (typeOK : Prop) (Af Df : List G) : Prop :=
  typeOK ∧ (Af = [] ∨ ∃ a ∈ Af, ev a) ∧ ¬ ∃ d ∈ Df, ev d

/-- **Field-level default deny**: a field with allows, none of which holds, is denied. -/
theorem field_default_deny {ev : G → Prop} {typeOK : Prop} {Af : List G} (Df : List G)
    (hne : Af ≠ []) (h : ∀ a ∈ Af, ¬ ev a) : ¬ fieldOK ev typeOK Af Df := by
  rintro ⟨_, hA | ⟨a, ha, hev⟩, _⟩
  · exact hne hA
  · exact h a ha hev

/-- **A field policy never grants a field operation the type denies.** -/
theorem field_le_type {ev : G → Prop} {typeOK : Prop} {Af Df : List G}
    (h : fieldOK ev typeOK Af Df) : typeOK := h.1

/-- With the type's union minus deny as `type_allowed`: no type allow holding denies every
field operation, whatever the field's own allows. -/
theorem field_type_default_deny {ev : G → Prop} {A D : List G} (Af Df : List G)
    (h : ∀ a ∈ A, ¬ ev a) : ¬ fieldOK ev (umd ev A D) Af Df :=
  fun hf => umd_default_deny D h (field_le_type hf)

/-- A field with only denies narrows the type's decision. -/
theorem field_denies_only (ev : G → Prop) (typeOK : Prop) (Df : List G) :
    fieldOK ev typeOK [] Df ↔ typeOK ∧ ¬ ∃ d ∈ Df, ev d := by
  simp [fieldOK]

/-- A field with allows needs one of them (and the type, and no field deny). -/
theorem field_with_allows {ev : G → Prop} {typeOK : Prop} {Af : List G} (Df : List G)
    (hne : Af ≠ []) :
    fieldOK ev typeOK Af Df ↔ typeOK ∧ (∃ a ∈ Af, ev a) ∧ ¬ ∃ d ∈ Df, ev d := by
  simp [fieldOK, hne]

/-- Once a field has an allow, adding another only widens. -/
theorem field_mono_allow {ev : G → Prop} {typeOK : Prop} {Af Df : List G} (hne : Af ≠ [])
    (a : G) (h : fieldOK ev typeOK Af Df) : fieldOK ev typeOK (a :: Af) Df := by
  obtain ⟨ht, hA | ⟨b, hb, hev⟩, hD⟩ := h
  · exact absurd hA hne
  · exact ⟨ht, Or.inr ⟨b, List.mem_cons_of_mem _ hb, hev⟩, hD⟩

/-- **Adding the first allow to a field can narrow it**: with the type allowing and no field
policies, the field is allowed; adding one allow that doesn't hold denies it. -/
theorem first_field_allow_narrows :
    fieldOK (id : Prop → Prop) True [] [] ∧ ¬ fieldOK (id : Prop → Prop) True [False] [] := by
  refine ⟨⟨trivial, Or.inl rfl, by simp⟩, ?_⟩
  rintro ⟨_, h | ⟨a, ha, hev⟩, _⟩
  · exact List.cons_ne_nil _ _ h
  · rw [List.mem_singleton.mp ha] at hev; exact hev

/-- The reading "a field with allows *replaces* the type's allows": the field's allows stand in
for the type's, and both deny lists still apply. -/
def replaceOK (ev : G → Prop) (D Af Df : List G) : Prop :=
  (∃ a ∈ Af, ev a) ∧ (¬ ∃ d ∈ D, ev d) ∧ ¬ ∃ d ∈ Df, ev d

/-- **The formula isn't "replace"**: a type whose only allow fails, with a field allow that
holds. The replacing reading grants the field operation; §6.1's formula denies it. -/
theorem field_not_replace :
    replaceOK (id : Prop → Prop) [] [True] [] ∧
      ¬ fieldOK (id : Prop → Prop) (umd id [False] []) [True] [] := by
  refine ⟨⟨⟨True, List.mem_singleton_self _, trivial⟩, by simp, by simp⟩, ?_⟩
  rintro ⟨⟨⟨a, ha, hev⟩, _⟩, _⟩
  rw [List.mem_singleton.mp ha] at hev; exact hev

/-! ## The Ent port -/

/-- An ordered rule. -/
inductive Rule (G : Type*)
  | allow (g : G)
  | deny (g : G)

/-- Ordered first match with default deny: the first rule whose guard holds decides. -/
def firstMatch (ev : G → Prop) : List (Rule G) → Prop
  | [] => False
  | .allow g :: rs => ev g ∨ (¬ ev g ∧ firstMatch ev rs)
  | .deny g :: rs => ¬ ev g ∧ firstMatch ev rs

/-- Ported guards: a deny carries the negation of every earlier allow. -/
inductive PGuard (G : Type*)
  | base (g : G)
  /-- `D_j ∧ ¬(∨ A_i)` over the given earlier allows -/
  | guarded (d : G) (earlier : List G)

def PGuard.ev (ev : G → Prop) : PGuard G → Prop
  | .base g => ev g
  | .guarded d as => ev d ∧ ¬ ∃ a ∈ as, ev a

/-- The allows of a rule list, in order. -/
def allows : List (Rule G) → List G
  | [] => []
  | .allow g :: rs => g :: allows rs
  | .deny _ :: rs => allows rs

/-- The rewritten denies, given the allows seen so far. -/
def portDenies (seen : List G) : List (Rule G) → List (PGuard G)
  | [] => []
  | .allow g :: rs => portDenies (seen ++ [g]) rs
  | .deny d :: rs => .guarded d seen :: portDenies seen rs

/-- **The port**: every allow kept, every deny rewritten to `D'_j = D_j ∧ ¬(∨_{i<j} A_i)`. -/
def port (rs : List (Rule G)) : List (PGuard G) × List (PGuard G) :=
  ((allows rs).map .base, portDenies [] rs)

/-- The generalized statement, with allows `seen` already passed (and all false). -/
theorem port_aux (ev : G → Prop) :
    ∀ (rs : List (Rule G)) (seen : List G), (∀ a ∈ seen, ¬ ev a) →
      (firstMatch ev rs ↔ (∃ a ∈ allows rs, ev a) ∧
        ¬ ∃ d ∈ portDenies seen rs, PGuard.ev ev d) := by
  intro rs
  induction rs with
  | nil => intro _ _; simp [firstMatch, allows]
  | cons r rs ih =>
    intro seen hseen
    cases r with
    | allow g =>
      by_cases hg : ev g
      · -- the allow fires; every later rewritten deny is blocked by `g`
        simp only [firstMatch, hg, true_or, allows, List.mem_cons, true_iff]
        refine ⟨⟨g, Or.inl rfl, hg⟩, ?_⟩
        rintro ⟨d, hd, hev⟩
        -- every guard in `portDenies (seen ++ [g]) rs` negates `g`
        have key : ∀ (rs : List (Rule G)) (s : List G), g ∈ s →
            ∀ d ∈ portDenies s rs, ¬ PGuard.ev ev d := by
          intro rs
          induction rs with
          | nil => intro s _ d hd; simp [portDenies] at hd
          | cons r rs ih' =>
            intro s hs d hd
            cases r with
            | allow g' => exact ih' (s ++ [g']) (List.mem_append_left _ hs) d hd
            | deny d' =>
              simp only [portDenies, List.mem_cons] at hd
              rcases hd with rfl | hd
              · exact fun h => h.2 ⟨g, hs, hg⟩
              · exact ih' s hs d hd
        exact key rs (seen ++ [g]) (List.mem_append_right _ (List.mem_singleton_self g)) d hd hev
      · have hseen' : ∀ a ∈ seen ++ [g], ¬ ev a := by
          intro a ha
          rcases List.mem_append.mp ha with ha | ha
          · exact hseen a ha
          · rw [List.mem_singleton.mp ha]; exact hg
        rw [show firstMatch ev (.allow g :: rs) ↔ firstMatch ev rs by simp [firstMatch, hg],
          ih (seen ++ [g]) hseen']
        simp [allows, portDenies, hg]
    | deny d =>
      have hd' : PGuard.ev ev (.guarded d seen) ↔ ev d := by
        simp only [PGuard.ev, and_iff_left_iff_imp]
        rintro _ ⟨a, ha, hev⟩; exact hseen a ha hev
      by_cases hd : ev d
      · simp only [firstMatch, hd, not_true_eq_false, false_and, false_iff, not_and, not_not,
          allows, portDenies]
        exact fun _ => ⟨_, List.mem_cons_self, hd'.mpr hd⟩
      · simp only [firstMatch, hd, not_false_eq_true, true_and, allows, portDenies,
          List.mem_cons, exists_eq_or_imp, hd', false_or]
        exact ih seen hseen

/-- **The Ent port is exact**: ordered first-match rules (default deny) decide exactly as
union minus deny over the kept allows and the rewritten denies `D'_j`. -/
theorem port_exact (ev : G → Prop) (rs : List (Rule G)) :
    firstMatch ev rs ↔ umd (PGuard.ev ev) (port rs).1 (port rs).2 := by
  rw [port_aux ev rs [] (by simp)]
  simp [umd, port, PGuard.ev]

theorem portDenies_allows : ∀ (as s : List G), portDenies s (as.map Rule.allow) = []
  | [], _ => rfl
  | a :: as, s => by simpa [portDenies] using portDenies_allows as (s ++ [a])

/-- Rules whose denies all come first: the rewrite changes nothing (each `D'_j` negates an
empty list). -/
theorem port_denies_first (ev : G → Prop) (ds as : List G) :
    firstMatch ev (ds.map .deny ++ as.map .allow) ↔ umd ev as ds := by
  rw [port_exact]
  have hA : ∀ ds : List G, allows (ds.map Rule.deny ++ as.map Rule.allow) = as := by
    intro ds
    induction ds with
    | nil =>
      simp only [List.map_nil, List.nil_append]
      induction as with
      | nil => rfl
      | cons a as ih => simp [allows, ih]
    | cons d ds ih => simpa [allows] using ih
  have hD : ∀ ds : List G, portDenies [] (ds.map Rule.deny ++ as.map Rule.allow) =
      ds.map (fun d => PGuard.guarded d []) := by
    intro ds
    induction ds with
    | nil =>
      simp only [List.map_nil, List.nil_append]
      exact portDenies_allows as []
    | cons d ds ih => simp [portDenies, ih]
  simp [umd, port, hA, hD, PGuard.ev]

/-- **Ent's own default.** entgo treats "every rule skips" as allow. Porting such a policy
needs a final `allow true` as well; with it, the port is exact. -/
def entMatch (ev : G → Prop) : List (Rule G) → Prop
  | [] => True
  | .allow g :: rs => ev g ∨ (¬ ev g ∧ entMatch ev rs)
  | .deny g :: rs => ¬ ev g ∧ entMatch ev rs

theorem entMatch_eq_firstMatch_append (ev : G → Prop) (t : G) (ht : ev t) :
    ∀ rs : List (Rule G), entMatch ev rs ↔ firstMatch ev (rs ++ [.allow t]) := by
  intro rs
  induction rs with
  | nil => simp [entMatch, firstMatch, ht]
  | cons r rs ih =>
    cases r <;> simp [entMatch, firstMatch, ih]

theorem port_entDefault (ev : G → Prop) (t : G) (ht : ev t) (rs : List (Rule G)) :
    entMatch ev rs ↔
      umd (PGuard.ev ev) (port (rs ++ [.allow t])).1 (port (rs ++ [.allow t])).2 := by
  rw [entMatch_eq_firstMatch_append ev t ht, port_exact]

end PolicyCombine
end Shep
