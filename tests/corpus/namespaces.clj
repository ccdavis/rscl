;; Namespaces: ns, require with :as and :refer, *ns*, and a -main in a named ns.
(ns demo.main
  (:require [mylib.util :as u :refer [shout twice]]
            [mylib.strings :as s]))
(println (u/double-it 21) (shout "hi") (s/wrap "x") s/greeting mylib.strings/greeting)
(twice (println "twice"))
(println (ns-name *ns*) (map str (sort (map key (ns-aliases *ns*)))))
(println (contains? (loaded-libs) 'mylib.util) (bound? #'shout) (ns-name (find-ns 'mylib.strings)) (find-ns 'nope))
(require 'mylib.strings)
(println "second require is a no-op")
(require '[clojure.string :as str])
(println (str/upper-case "builtin ns") (contains? (loaded-libs) 'clojure.string))
(require 'mylib.double-load)
(require 'mylib.double-load :reload)
(in-ns 'other.ns)
(def here 1)
(in-ns 'demo.main)
(println other.ns/here (ns-resolve 'other.ns 'here) (resolve 'u/double-it))
(defn -main [& args] (println "main in" (ns-name *ns*) args))
