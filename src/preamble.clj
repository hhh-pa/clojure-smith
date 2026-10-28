;; Safe-math wrappers and checksum helpers emitted into every generated
;; program. These are total and deterministic: long arithmetic wraps around
;; on overflow (two's complement) instead of throwing, and every accessor is
;; bounds-checked with a caller-supplied default.
(defn s+ [x y] (unchecked-add x y))
(defn s- [x y] (unchecked-subtract x y))
(defn s* [x y] (unchecked-multiply x y))
(defn s-neg [x] (unchecked-negate x))
(defn s-inc [x] (unchecked-inc x))
(defn s-dec [x] (unchecked-dec x))
(defn s-quot [x y] (if (zero? y) x (quot x y)))
(defn s-rem [x y] (if (zero? y) x (rem x y)))
;; Total double division: raw `/` resolves `Numbers/divide` by static type,
;; and the (Object, Object) overload throws "Divide by zero" on a zero
;; divisor. Retyping both sides to primitive double selects the IEEE path
;; (Inf/NaN, never throws).
(defn s-div [x y] (let [a (double x) b (double y)] (/ a b)))
;; Total double-to-long conversion: `long` throws on infinities and on
;; finite doubles outside the long range (NaN converts to 0 natively, so it
;; safely falls through to the :else branch).
(defn s-long [x]
  (cond (infinite? x) 0
        (>= x 9.223372036854776e18) 9223372036854775807
        (<= x -9.223372036854776e18) -9223372036854775808
        :else (long x)))
(defn s-nth [v i d] (if (and (<= 0 i) (< i (count v))) (nth v i) d))
(defn s-peek [v d] (if (empty? v) d (peek v)))
(defn s-assoc-v [v i x] (if (and (<= 0 i) (< i (count v))) (assoc v i x) v))
(defn s-subvec [v a b]
  (let [n (count v)
        lo (min (max a 0) n)
        hi (min (max b lo) n)]
    (vec (subvec v lo hi))))
(defn s-pop [v] (if (empty? v) v (pop v)))
;; Total take/drop: the built-ins cast the count to int and throw on huge
;; longs, so clamp into the int range first (equivalent for all realizable
;; collections, which are far below 2^31 elements).
(defn s-take [n v] (vec (take (min (max n 0) 2147483647) v)))
(defn s-drop [n v] (vec (drop (min (max n 0) 2147483647) v)))
(defn s-get [m k d] (if (contains? m k) (get m k) d))
(defn s-subs [s a b]
  (let [n (count s)
        lo (min (max a 0) n)
        hi (min (max b lo) n)]
    (subs s lo hi)))
;; FNV-1a (64-bit) over the printed representation. Implemented by hand so
;; the checksum never depends on clojure.core/hash, whose algorithm could
;; legitimately change between Clojure versions.
(defn smith-hash [x]
  (let [s (pr-str x)
        n (count s)]
    (loop [i 0 h -3750763034362895579]
      (if (< i n)
        (recur (unchecked-inc i)
               (unchecked-multiply (bit-xor h (long (int (nth s i)))) 1099511628211))
        h))))
