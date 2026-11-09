(println (count (map keyword (map str (range 20000)))) (= (keyword "zz") :zz) (zipmap (map (fn [i] (keyword (str "k" i))) (range 50)) (range 50)))
